# Paired real-checkout load gate

This opt-in Linux harness runs **real AFT binaries**, either standalone NDJSON or
`aft --subc` behind the loopback `views_perf_daemon` example. It never connects to
the operator's daemon, installs a build on their Mac, or changes product settings.
A Linux pass is necessary evidence, not a claim about macOS performance.

## Budgets (unchanged)

For each of grep, glob, search (`aft_search`), read, outline, zoom and callgraph:
20 or more samples, **p95 <1,000 ms**, **max <5,000 ms**. p99 is reported, not gated.
The 240-second steady window requires zero errors, refusals, protocol errors or
timeouts; AFT CPU **<0.8 core** averaged over complete steady intervals; busy
fraction **<0.5** (time at >=0.9 core); and dispatched arrivals **>=98%** of the
requested rate. Equality fails the upper bounds. CPU sums all AFT threads and is
not divided by the allocation. Startup and drain are not steady CPU intervals.

These interactive budgets leave room inside a three-second worker launch deadline
and forbid multi-second stalls or a sustained verification loop. Do not loosen
arguments to bless an unhealthy baseline. Client backlog is part of end-to-end
latency; expired, refused and failed calls stay in the percentiles. Per-tool error,
refusal and backlog statistics are printed next to latency. A missing tool,
missing telemetry or unrun steady window cannot pass.

Peak process RSS (`VmHWM`) and sampled current RSS (`VmRSS`) are retained. Phase
peaks use **current RSS**, not the process-lifetime HWM inherited from startup.
`views-startup-memory` is a separately named **report-only** reference (default
1,024 MiB, configurable with `--views-startup-memory-mib`); it does not affect
exit status. This is a provisional comparison number, not an approved deployment
budget: the views graph-memory fix and a reviewed memory threshold are pending.

## Readiness and lifecycle

Readiness must be witnessed by a successful **indexed query** disclosing its own
`trigram: ready` lane (structured `lanes.trigram.status` or the public text footer),
and a successful public callers query. A filesystem fallback, legacy resident
`status.search_index`, the older view-runtime generation/pin status, or a
publication log cannot bless lexical
readiness. Complete=false due to a result cap or disabled semantic lane is not an
unready trigram index. Full readiness responses and elapsed times are saved.

The lifecycle cases are never pooled:

* `cold-alone`: fresh process/state, readiness probes only, no agent workload.
  Unlike a passive status check, probes may trigger routed indexing;
  their cost belongs to startup. This is a readiness measurement, not a latency pass.
* `in-process-warm`: independent fresh process/state; readiness, then the full
  steady window **in the same process**.
* `warm-restart`: new process with the preceding in-process case's persisted state;
  confirm readiness again, then a full steady window. It requires `--state`.

The optional diagnostic `cold-traffic` case starts arrivals alongside readiness;
it is not substituted for any of the three matrix cases. Cold means fresh AFT
state, **not** a dropped kernel page cache. No synthetic files or CPU load are used.
Semantic and inspect/LSP work are disabled; trigram, callgraph and watchers stay on.

## Provision, build and run in one job

Use the shell tool's `runon: "linux,12c,net"` selector for jobs compiling AFT:
the release compiler needs the larger memory allocation. Record that allocation;
for comparisons to eight-CPU runs, restrict runtime affinity to eight of the
allocated CPUs **after** building, identically for both binary arms. CPU remains
measured in absolute cores, with the same 0.8-core budget. Never build or load-test
on the operator's Mac.
The fake daemon's `state/connection.json` must have no group/world-writable ancestor. Put results
under a newly created private directory in the checkout, not shared `target/` or
`.cortexkit/` mounts (which may be group-writable on the server); the
production transport intentionally refuses insecure connection-file parents.
Build-server snapshots do not preserve ignored binaries/state between jobs.
Commit the harness first and print `git log -1` at job start. Use a writable,
exact-revision clone as the subject if the snapshot's Git index is read-only.
Keep real `target/`, installed `node_modules/`, and the four pinned corpus clones
(ripgrep, flask, fastify and turborepo, from `benchmarks/aft-search/corpus/corpus.toml`)
in that subject; do not copy a target directory into itself.

```sh
git log -1 --oneline
cargo build --offline --release -p agent-file-tools --bin aft --example views_perf_daemon -j 12
bun install --frozen-lockfile
python3 benchmarks/aft-search/provision_corpus.py
python3 benchmarks/load-check/run.py --self-test

# Baseline and candidate are prebuilt Linux binaries. Both arms run serially
# against exactly the same subject, deterministic arrivals and CPU allocation.
mkdir -m 700 .load-check-private
python3 benchmarks/load-check/run.py \
  --repo /path/to/populated-subject \
  --baseline /path/to/baseline/aft --binary /path/to/candidate/aft \
  --broker target/release/examples/views_perf_daemon \
  --rate 600 --duration 240 --out .load-check-private/results
```

`--baseline` selects the complete paired matrix: standalone/subc, views off/on,
baseline/candidate, and all three lifecycle cases (24 phase records, 16 steady
windows). Independent state prevents one binary from warming the other's stores.
Both see the same real subject, selected inputs, arrival spacing, session count,
in-flight cap and affinity mask. Matrix checkpoints are written after each phase;
per-phase failures do not stop the other cases. For a harness-only current-main
validation, baseline and candidate may intentionally be identical binaries; record
that fact and their hashes rather than claiming a product regression comparison.
For a deployment comparison, supply the actual installed baseline binary.

A single case omits `--baseline` and accepts `--transport standalone|subc`,
`--views off|on`, `--phase`, and `--state`. `--broker` is required for subc. The
pipelined broker has one socket reader and one writer, accepts new requests while
others are pending, and correlates out-of-order completions by ID. Each session
has a successfully acknowledged RouteBind before probes/traffic. Child identity,
binds, startup stderr, child logs and readiness witnesses are retained separately
from standalone results. A bind ACK is not index readiness.

Outputs: `matrix.json`, per-case `result.json[.gz]`, `summary.md`,
`daemon.log[.gz]`, and subc `broker.log[.gz]`. JSON retains every scheduled call,
arguments, response excerpts/errors, client queue, CPU/RSS samples, binary hash,
subject revision/disk footprint, affinity, configuration and actual durations.
Optional `--profile-on-failure` collects a separate 30-second diagnostic load;
it does not replace or rerun the measured window. Preserve results outside build
directories. Reports belong under `.cortexkit/internal-docs/investigations/`,
uncommitted; only harness source and the broker example belong in Git.

Steady exit status: 0 PASS, 1 FAIL; setup errors are also failures. Cold-alone is
measurement-only, but its readiness boolean must still be inspected. Do not pool
transport, views mode, binary arm or lifecycle percentiles to hide a failed case.
