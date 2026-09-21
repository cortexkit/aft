# Semantic embed build RSS investigation — September 2026 (issue #327)

## Result

**Not reproduced, and the mechanism is not named.** A daemon-level harness that
records resident set against embed batch number was built and run against four
binaries — v0.56.0, v0.56.1, v0.56.2 and the 0.57.0 development head — on both
macOS and Linux, on both embedding backends, at the reporter's own corpus
scale of 44,200 files and 5,525 embed batches, and across nine corpus content
classes including a backend that refuses oversized rows. Every run was flat:
resident set rose linearly with embedded chunks at the index's own per-chunk
cost and stopped when the build stopped. Nothing resembling the reported
0.5–3 GB/min appeared in any of them.

The closest run to theirs settles at 2.4 GB, the same order as the 1.4–1.9 GB
band they themselves measured as *stable* on v0.56.0 — somewhat above it, which
is what a slightly different corpus should produce. At the batch number where
their failing run was at 16.2 GB, this one is at 0.89 GB.

That is a negative result, not a fix. What it buys is a large, measured
exclusion and a harness that runs on Linux, in a container, on either backend,
against any of several corpus content classes. Two artifacts ship as guards:
a regression test bounding the embed loop's working set per chunk, and a second
one bounding it again with the backend refusing rows, which is the path issue
#318 added and no earlier arm had ever executed.

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

Corpus content and backend strictness are separate knobs, added once content
became the last unmoved variable:

- `--corpus` — content class of the semantically indexed files, so the chunker
  and the embed loop see Cyrillic, dense XML or base64 rather than uniform ASCII
- `--sidecars` — files carrying extensions the semantic index does not accept,
  which are walked, watched and trigram-indexed but never chunked
- `--reject-over-tokens` — answer `exceed_context_size_error` above a limit,
  which is what drives the recursive bisection and row shrinking from #318
- `--smaps` — capture `/proc/<pid>/smaps_rollup` beside every sample on Linux,
  which is what separates anonymous heap from mapped artifacts

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

## What the reporter's own instrumented run settled

The reporter later ran v0.56.2 against their 44,200-file corpus with the
remote backend and attached the daemon log, a 30-second resident-set track and
two `smaps_rollup` snapshots four minutes apart. Four things in it are
conclusions rather than hypotheses, and each closes a line of inquiry:

- **It is anonymous heap.** Between the snapshots `Pss_Anon` and
  `Private_Dirty` went 17.97 → 22.77 GB while `Pss_File` — everything mapped
  from disk — went 41.9 → 7.7 MB. Mapped index artifacts are a rounding error,
  so every hypothesis about resident mapped artifacts or page-cache accounting
  is dead.
- **Only one build was ever live.** Exactly one `build_started` appears in the
  whole log. The concurrent-semantic-builds hypothesis this harness could not
  rule out on a synthetic corpus is refuted by their own data.
- **Collection is not the balloon.** The collect phase reported 375,756 chunks
  from 37,212 files in 10,945 ms, about four and a half minutes before the
  memory readings even begin.
- **The growth has a ceiling.** The track reaches roughly 22 GB and then
  oscillates between 20.9 and 23.3 GB rather than continuing to climb. And
  `Swap` went 0 → 11.54 GB between the snapshots, so committed anonymous memory
  is around 34 GB and the host was paging by the end. A plateau under paging
  pressure is not the shape of an unbounded per-batch accumulator, and any
  hypothesis that predicts unbounded growth is contradicted by the track.

What it leaves is a rate: batch 125 of 5,872 reached in about 9.5 minutes while
resident set went 0.6 → 22.8 GB, or roughly **178 MB per 64-chunk batch**, and
as much as **310 MB per batch** across the steepest twenty-batch window. Against
the 0.19–0.23 MB/batch every arm here measures, that is three orders of
magnitude.

## Content as the variable, and the overflow recovery path

The scale run above matched their file count and batch count with uniform ASCII
Rust files. It did not match their content, and content was the last thing the
harness had never moved. The reporter names the classes: real Cyrillic
`.properties`, dense XML, and base64 literals — the same classes that produced
#318, where llama.cpp rejected oversized rows with HTTP 400 and the remedy was
recursive batch bisection plus row shrinking.

That remedy is the sharpest available suspect, because it is the one path in
the embed loop that does *more* work per batch the more rows are oversized, and
because no arm in this investigation had ever executed it: every stub ever
pointed at the daemon accepted every row. So the harness gained a stub that
answers `exceed_context_size_error` above a configurable limit, and the arms
that trip it were run first.

Nine arms, 1,200 files and 20 symbols each, one tool call per second, 300 ms of
backend latency per batch, all on the 0.57.0 development head on macOS. Only
the corpus content and the backend's context limit differ between them. The
slope is fitted across the embed phase with the final batch excluded, because
the finished index is written to disk once the last batch lands and that
transient belongs to persistence rather than to the embed loop; the second
column includes it so the exclusion is visible rather than assumed.

| arm | corpus | batches | MB/batch | with the index write | peak | settled |
| --- | --- | --- | --- | --- | --- | --- |
| cyrillic-recovery | 48.2 MB | 413 | 0.192 | 0.295 | 630 MB | 554 MB |
| java-recovery | 25.2 MB | 413 | 0.134 | 0.225 | 588 MB | 406 MB |
| rust-baseline | 10.6 MB | 375 | 0.023 | 0.078 | 433 MB | 433 MB |
| java-ascii | 25.2 MB | 413 | 0.527 | 0.643 | 633 MB | 633 MB |
| cyrillic | 48.2 MB | 413 | 0.288 | 0.431 | 668 MB | 668 MB |
| xml | 91.6 MB | 413 | 0.229 | 0.297 | 450 MB | 450 MB |
| base64 | 82.8 MB | 413 | -0.040 | 0.072 | 419 MB | 417 MB |
| mixed | 74.2 MB | 413 | 0.203 | 0.282 | 485 MB | 473 MB |
| sidecar-mixed | 71.2 MB | 413 | 0.312 | 0.459 | 656 MB | 619 MB |

Every arm sits in one 0.02–0.53 MB/batch band, which is the same band as every
earlier arm and three hundred to thirteen thousand times below the reported
rate. The `base64` arm's slightly negative slope is the operating system
reclaiming pages during the run, not a corpus that frees memory. **No content
class reproduces anything resembling the report.**

### The recovery path ran, heavily, and retained less than not running it

The two recovery arms are the important ones, and they are not vacuous: the
rejecting stub answered 51,187 and 76,287 context-overflow 400s, and the request
count per batch went from 1 to 188 and 249 respectively as batches bisected down
to single rows and those rows were shrunk and retried. The path ran on
essentially every batch of both runs.

It produced the *lowest* slopes of any Java-corpus arm — 0.192 and 0.134 against
0.527 for the same corpus with the limit removed. That direction is not an
anomaly: a shrunk row stores less text in the index than the row it replaced, so
a build that shrinks most of its rows ends up smaller.

The daemon-level figure is confirmed exactly by
`crates/aft/tests/semantic_embed_overflow_recovery_working_set_test.rs`, which
builds the same corpus twice against the same stub and counts live heap bytes
with an allocator local to the test binary, so there is no operating-system page
accounting in the number at all:

| backend | bytes retained per chunk at the peak | requests | rejections |
| --- | --- | --- | --- |
| accepts every row | 3,886 | 38 | 0 |
| enforces a context limit | 3,416 | 7,162 | 4,762 |

**0.88×.** Recursive bisection holds one sub-batch per level while it descends
and drops it as the recursion unwinds, and shrink retries replace a row rather
than accumulating beside it. The hypothesis that the #318 recovery path
accumulates per bisection level is measured, and it is wrong.

### Why no content class produces an oversized row at the reporter's settings

The harness records the widest row each arm actually sent, which turns out to
explain the flatness rather than merely accompany it. With
`max_input_tokens: 512` — the reporter's setting — every arm's widest row is the
same **1,097 bytes**, whatever the content class:

| arm | widest row, characters | widest row, bytes | tokens |
| --- | --- | --- | --- |
| rust-baseline | 594 | 594 | 170 |
| java-ascii, xml, base64, sidecar-mixed | 1,097 | 1,097 | 314 |
| cyrillic, mixed | 744 | 1,097 | 466 |

The reason is in how the caps are applied. The body cap is compared and sliced
against byte length (`body.len() > caps.body_chars` in
`crates/aft/src/semantic_index.rs`), while the signature cap and the whole-row
clamp go through `truncate_chars` and count characters. So multi-byte content
does not produce a longer row — it produces the *same* row length in bytes with
fewer characters in it. Cyrillic is the only class whose token count diverges,
and only by 1.48× (466 against 314 for an identical 1,097 bytes), which does not
cross a 512-token window.

That is worth stating plainly because it is the opposite of what the content
hypothesis predicted: **at the reporter's own configuration, a Cyrillic body
cannot produce a row that overflows a 512-token backend.** The route to #318's
overflow has to be the signature, which is capped in characters and so can carry
800 bytes of Cyrillic, or a tokenizer that splits their text more finely than
the one modelled here. Either way the recovery arms above show that reaching
the path costs nothing in retained memory.

### Files the semantic index never reads

The classes the reporter names live mostly in `.properties`, `.xml` and `.bpmn`
files, and `is_semantic_indexed_extension` accepts none of those — they never
become chunks and never reach an embed batch at all. They are still walked,
watched and trigram-indexed, so they can cost memory in a plane that merely runs
beside the embed build. The `sidecar-mixed` arm adds 2,400 such files (46 MB of
Cyrillic `.properties`, deeply nested single-line XML and base64-valued JSON)
to an otherwise unchanged ASCII Java corpus. It is flat too, at 0.312 MB/batch.

### How the arms were run

`docs/investigations/scripts/semantic-embed-rss-content-arms.sh` fixes file
count, symbol count, batch size, backend latency, sampling interval and
tool-call load for every arm and varies only the content flags, so a difference
between two arms has one candidate cause.
`semantic-embed-rss-content-arms-report.py` tables them from the saved samples,
which are kept in `docs/investigations/data/semantic-embed-rss-content-2026-09/`
so the table can be rederived rather than believed.

`smaps_rollup` was **not** captured for these arms. The harness takes it beside
every sample under `--smaps`, but no arm grew enough for the anonymous-versus
-mapped split to have anything to separate, and these runs are on macOS where
`/proc` does not exist. Any future arm that does balloon should be rerun in the
Linux container with that flag.

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
| content classes | nine arms, 413 batches each | 0.02–0.53 MB/batch |
| backend refusing oversized rows | 413 batches, 51,187 rejections | 0.192 MB/batch |

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

## The regression guards

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

`crates/aft/tests/semantic_embed_overflow_recovery_working_set_test.rs` applies
the same technique to the path the first test cannot reach, because its stub
accepts everything. This one runs a Cyrillic-dense corpus twice against the same
stub and changes exactly one thing: whether the stub enforces a context limit.
It bounds retention per chunk at the same 4 KB and additionally bounds the
*ratio* between the two runs at 3×, so an accumulator that only appears while
bisecting has something to trip even if the absolute figure stayed under the
ceiling.

The limit is deliberately set below what the corpus produces. The question that
arm answers is what recovery costs when most batches reach it, not whether one
synthetic corpus crosses a particular server's window — and a limit no row
crossed would leave the comparison passing while measuring nothing. Three
assertions guard against exactly that: the corpus must produce a row past the
limit, rejections must outnumber batches, and the enforcing run must issue more
than twice the requests of the accepting one. Raising the limit above the
corpus's widest row turns the test red on the first of them.

## Measurement caveats

- Every macOS number here is arm64. The Linux runs are linux/amd64 under
  emulation, except the local ONNX lane and the reporter-scale run, which are
  native linux/arm64 so that inference and parsing run at full speed. The
  reporter is on linux x64. The nine content arms are macOS arm64: content is
  the variable they move, so the operating system is deliberately held where the
  earlier macOS arms had it rather than changed alongside it.
- The stub's token model is an approximation, not a tokenizer. It charges ASCII
  at three and a half characters per token and non-ASCII characters about a
  token each, which is how an English WordPiece vocabulary behaves, but it does
  not model entropy — so high-entropy ASCII such as base64 is charged the
  ordinary English rate even though a real tokenizer splits it far more finely.
  The recovery arms therefore reach the overflow path by lowering the limit
  rather than by relying on that model to be exact.
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
and batch count, nine corpus content classes including the three they name, the
overflow recovery path under a backend that refuses most rows, files the
semantic index never reads, and four binaries spanning both reported arrival
windows. None of it grows.

What is still untested:

1. **Drive the daemon through the subc bridge rather than standalone NDJSON.**
   The reporter's daemon runs under the plugin, and the bridge transport is the
   one layer this harness does not exercise. `bridge.request_timeout_ms` and
   `hang_threshold` appear in their config and have no analogue here. The
   integration suite already stands a subc daemon up in-process
   (`crates/aft/tests/integration/subc_bridge_test.rs`), so this is reachable
   without the live fleet.
2. **Replay their backend's actual HTTP responses.** Every arm here answers with
   a compact stub body. A real llama.cpp server's response stream, and whatever
   the client does with it, is the remaining piece of the embed lane that is
   modelled rather than reproduced.
3. **Their actual files, rather than a model of them.** Scale was matched in the
   previous round and content in this one, and both came back flat. If the
   accumulator keys on something about their tree that neither exercise thought
   to model, only their tree will show it.

A bisect over the 27-commit v0.56.0→v0.56.1 range is only worth running once
one of those reproduces. The parent's correction stands and was re-verified: no
commit in that range touches `crates/aft/src/semantic_index.rs` or
`crates/aft/src/synapse_embed.rs`, and `Cargo.lock` moves only the two workspace
version strings, so no dependency changed under the remote lane either.

## The measurement to ask the reporter for

Guessing at further environment differences is now the expensive path, and this
round is the point at which to say so plainly: **content was the last variable
we could move on our own, and moving it changed nothing.** The difference is in
their environment, and the next move is a question to them rather than another
guess from us.

**The indexes-off run is still the one that discriminates**, and the reporter
has it queued. In their project config:

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
  wire behaviour — items 2 and 3 above.
- **If it does not leak**, the accumulator is in a plane that merely runs
  *alongside* the embed build — the trigram index, the callgraph store, Tier-2
  inspect, or the watcher over 44,200 files — and everything measured here has
  been looking in the wrong place. That is a different search entirely, and
  worth knowing before it costs another week.

Two further things this round makes worth asking for, both cheap:

- **A `smaps_rollup` and a `/proc/meminfo` from the *start* of the embed phase**,
  not only from inside the balloon. Their two snapshots are four minutes apart
  and both were taken after `Swap` had reached 11.54 GB, so the host was already
  paging when the baseline was set. A rollup at batch 10 would give the later
  ones something to be a difference from.
- **A few thousand representative files from the tree**, or a tarball of one
  module. We have now modelled the three content classes they named and none of
  them reproduces anything; if the shape that matters is something about their
  files nobody here thought to model, only their files will show it.
