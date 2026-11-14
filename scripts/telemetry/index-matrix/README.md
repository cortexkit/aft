# Fixed-memory index measurement matrix

This sibling of `../cost-gate.py` measures cold index builds and guarded
call-graph/query costs. The active matrix policy is **`meaning-nine-v2`** below:
six completed meaning-search projects are retained and only three more projects
build meaning search, inside their full build. It
imports the nightly gate's `oss-matrix.py` NDJSON framing and resource/phase
samplers. It does not change the nightly workflow, profile, fixtures or baselines.
No debug binaries, artificial corpus subsets, sampling of call sites, or inferred
call-shape counts are used.

## Fresh Ubuntu 24.04 x86-64 server

Use a dedicated machine with 16 cores, 64 GB RAM and at least 360 GB disk. Do not
run compilation, other repositories, or unrelated services during measurements.
Only public clones are needed; do not copy credentials or a developer HOME.

```sh
sudo apt-get update
sudo apt-get install -y build-essential pkg-config libssl-dev ca-certificates curl git python3 util-linux
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o rustup-init.sh
sh rustup-init.sh -y --profile minimal
. "$HOME/.cargo/env"
# Run from the checkout containing this README and the census feature.
git rev-parse HEAD
cargo build --release -p agent-file-tools
cp target/release/aft target/aft-default
cargo build --release -p agent-file-tools --features callgraph-shape-census
cp target/release/aft target/aft-shape
```

The optional census feature is included in this checkout. Never add it
to production/release workflows. If fat LTO is too slow, the repository's
`--profile ci-storm` is also optimized (`opt-level=3`, no debug assertions).
Use that profile **for both binaries**, copy from `target/ci-storm/`, and pass
`--build-profile ci-storm` below. Record `rustc --version`, `cargo --version`, the
source commit and binary hashes with the run. The harness records machine facts,
provided source commits, profile and SHA-256 hashes in every repository result.

The bare Rust executable needs ONNX Runtime installed; unlike the plugin, it does
not download the runtime. Install the public pinned x86-64 runtime:

```sh
mkdir -p target/index-matrix/runtime
cd target/index-matrix/runtime
curl -fL --retry 3 https://github.com/microsoft/onnxruntime/releases/download/v1.24.4/onnxruntime-linux-x64-1.24.4.tgz -o runtime.tgz
# Recorded official archive SHA-256 (Linux x86-64):
printf '%s  %s\n' 3a211fbea252c1e66290658f1b735b772056149f28321e71c308942cdb54b747 runtime.tgz | sha256sum -c -
tar -xzf runtime.tgz
export ORT_DYLIB_PATH="$PWD/onnxruntime-linux-x64-1.24.4/lib/libonnxruntime.so"
cd ../../..
```

The fastembed default model is `all-MiniLM-L6-v2`; network access to its public
Hugging Face files is needed. Every build has fresh model/cache directories, so
model download/initialization is part of the measured cold meaning run. No remote
embedding service is configured. Because standalone `runner` sessions do not
admit semantic roots automatically, the user-tier configure document explicitly
selects this checkout through `index.roots` with `search` and `semantic`. The
build JSON records this admission source; it does not impersonate an interactive
OpenCode/Pi session. Configure starts the fill without running a search query.
Text and meaning runs do not build a call graph.
Tests remain in the normal AFT corpus; Git/AFT ignore rules still apply. Git LFS
payloads and submodules are not populated, matching a plain shallow checkout.

## Run one repository or the whole matrix

Run from the AFT repository root. Use `matrix.py --revision meaning-nine-v2.1`
for the approved matrix phase plan, not an unfiltered standalone `run.py` call.
`--repository` on `run.py` selects one exact manifest name. A results-directory
lock prevents overlapping invocations. Keep results and checkouts under ignored
`target/index-matrix/`, **never commit them**.

For a real 48 GiB **resident-memory guard**, use a writable cgroup-v2 parent with
the memory controller delegated. On a dedicated root-owned Ubuntu host,
`/sys/fs/cgroup` normally has it enabled. Each AFT process joins a fresh cgroup
before exec, with `memory.max=51539607552` and `memory.swap.max=0`. The raw
`*.memory-events` records show OOM kills. Without `--cgroup-parent`, the harness
uses an exec-time `prlimit --as` address-space limit: conservative, not an RSS
measurement. An address-space failure must not be described as a measured 48 GiB
RSS overrun. Lower `--guard-gib` on a smaller host; it can never exceed 48.

```sh
python3 scripts/telemetry/index-matrix/run.py \
  --repository neovim/neovim --phases callgraph --query-build none \
  --plan-revision meaning-nine-v2.1 \
  --aft-binary target/aft-default --shape-binary target/aft-shape \
  --aft-source-commit "$(git rev-parse HEAD)" \
  --shape-source-commit "$(git rev-parse HEAD)" \
  --cgroup-parent /sys/fs/cgroup

# For the whole matrix, use the detached revision command below.

python3 scripts/telemetry/index-matrix/tables.py
python3 -m unittest discover -s scripts/telemetry/index-matrix -p 'test_*.py' -v
```

Reissue the same command to resume. A completed result is skipped only when its
manifest entry, binary/runtime/harness hashes, commits and measurement parameters
match. Explicit transitions from `all-meaning-v1` or `meaning-nine-v2` to
implementation `meaning-nine-v2.1` retain prior ready
observations only when binary/runtime hashes, commits, guards, deadlines, cache
policy and manifest entry match; original result JSON is saved under `history/`.
Other changed inputs require a new results directory. Incomplete attempts
remain as raw evidence; a retry uses new isolated storage, not an incremental
index. A failed guarded full build is a completed observation with **not
measured** call shape and queries, not an estimate. Without `--shape-binary`, those
two parts are explicitly not measured. Default readiness budget is 14,400 s per
build; `--timeout-s` changes it. Model/backend failures are not readiness.

Each repository folder contains its pinned checkout, `result.json` and timestamped
attempts. Each build attempt includes `build.json`, `status.jsonl`, the uncapped
AFT `memory.census` responses in `memory-census.jsonl`, `phase-memory.csv`, stderr,
and AFT's own `storage/logs/`. Census is observed every five seconds and at
readiness; these are component observations, **not component peak guarantees**.
Kernel VmHWM is sampled while the process is alive immediately before shutdown;
the kernel value includes peaks between polls. Wall time is monotonic nanoseconds
from process launch through readiness polling/census, excluding checkout and
shutdown. CPU time uses the gate's `/proc/<pid>/stat` accounting (clock-tick
precision). Store apparent and allocated bytes are separate from model files and
all-storage bytes. Raw status includes the store's path/counts.

## Call shape and query semantics

Extraction alone is not exact. In `dispatch.rs`, missing receivers and missing Go
packages go directly to `Resolver::unknown`, but Rust and Go also take that path
after resolved-method ambiguity. The optional feature classifies actual resolver
exits, including memoized hits, and writes exact `(language, member)` groups to
`AFT_CALLGRAPH_SHAPE_CENSUS_JSON` after a successful callgraph or full build. The harness
expects schema version 1, `groups: [{language, member, sites, targets}]`, and exact
`top_callers: [{path, symbol, callers}]` for the busiest ten symbols (or all if
fewer exist). Additional hook fields are retained in raw JSON. No graph build
finishing outside the guard supplies call-shape or latency numbers.

For each of the ten symbols, `callers`, `impact` and `trace_to` use today's tool
with `includeTests=true` and normal defaults. Each cold query uses a **new AFT
process** over the already-built full store; each is followed by ten identical
warm requests. This yields ten cold and 100 warm observations per operation on a
repository with ten symbols. Default cold means process/query-cache cold, **not
OS-page-cache cold**. On a privileged dedicated host, `--drop-caches` runs `sync`
and writes Linux `drop_caches=3` before each cold launch. Cold setup/readiness is
outside the timed request. Raw request responses and integer latency nanoseconds
are in `queries.jsonl`; timeouts/failures are not silently counted as successes.

`tables.py` emits `repositories.csv` (one row per manifest entry),
`call-shape.csv` (per repository/language), `query-latency.csv`, `tables.json` and
`tables.md`. Missing measurements are empty cells with reasons, never invented
zeros. Percentiles are nearest-rank p50/p95/p99, without rounding raw units. Group
sizes mean **sites sharing an unknown answer**; targets per answer are also
reported separately. Language tables do not merge independent project namespaces.

## Pins and checkout sizes

`manifest.json` pins the default heads observed during preparation. File counts
come from a complete `git ls-tree -r` at each SHA after a shallow blobless clone;
no truncated GitHub API trees or GitHub language percentages. Counts describe
tracked file paths by the explicit case-sensitive extension map in `pin.py`,
including tests and vendors, not AFT's ignore-filtered indexed counts. Ambiguous
`.h` files are separate. The canonical GitLab source is gitlab.com, not GitHub.

Checkout sizes are deliberately `null` until actual measurement checkouts. The
runner then records real `du -sb --exclude=.git`, allocated bytes and bytes with
`.git` in `result.json`/tables. It does not pretend blobless clone size is checkout
size. To deliberately refresh all pins, run `python3
scripts/telemetry/index-matrix/pin.py`; `--resume` preserves existing pins.

Chromium was retained instead of LLVM: a plain `git clone --depth 1` completed
inside the 300 s probe budget on Linux, with 6,034,950,864 bytes including `.git`.
The 30 s polling probe observed completion at 240.0028163800016 s (an upper bound,
not exact clone CPU time). This is practical within the 360 GB disk budget.
The manifest independently pins Chromium's subsequently observed default head.

## Revised matrix: meaning-nine-v2.1

The original series started at **2026-11-13T23:30:40.416178Z** and was stopped
cleanly at **2026-11-14T05:33:09Z** to apply the nine-project meaning budget.
The phase policy remains `meaning-nine-v2`; implementation revision
`meaning-nine-v2.1` also distinguishes process exit before census export from a
24-hour deadline. It preserves the exit status and raw evidence. On a query retry,
the incomplete batch is renamed to `queries-retired-*.jsonl` and replayed whole;
only the new `queries.jsonl` contributes percentiles. Completed batches with
identical inputs are retained without replay. The service remains
`aft-index-matrix.service`; remote/local result paths are unchanged. `matrix-plan.json` records the revision, per-project phase plans,
revised UTC time, source/input hashes and previous plan. Existing results are
retained, with phase-specific artifact paths and revision labels in the tables.

The execution order is:

1. TensorRT-LLM first: retain its ready text result, discard only its unfinished
   meaning build after preserving raw evidence, then callgraph and queries.
2. The other eleven no-meaning projects, smallest tracked-path count first:
   Spark, Home Assistant, Swift, Magento, Node, .NET, Rust, Linux, Kotlin, GitLab,
   Chromium. Each runs text, callgraph, and queries on the callgraph store.
3. VS Code, Kubernetes, Elasticsearch: text, callgraph, full, then queries on
   the full store. There is **no separate meaning-only phase**. The full build's
   raw per-plane census gives the meaning component observations, not an
   independently measured component peak.
4. The six finished projects (MLX, Neovim, Perl, Superset, Solidity, Zig): preserve
   all previous results and queries; add only a new isolated callgraph phase.

`callgraph` always enables text plus call graph, with meaning **off**, and uses
the census binary. All existing query operations (`callers`, `impact`, `trace_to`)
need only the call graph; their warm/cold setup also leaves meaning off when the
source build is callgraph. Meaning-dependent queries/phases excluded by the plan
are explicitly `skipped` with a reason, never reported as zero latency.

The 48 GiB cgroup guard, kernel `memory.peak`, VmHWM, drop-caches policy and
86,400-second build deadline remain. A guard kill records `guard_killed`; a
build deadline records `deadline`, preserves evidence, and moves on. Terminal
VmHWM remains empty if `/proc` has disappeared; cgroup peak includes charged file
cache and is not substituted for process RSS.

The phase-policy restart estimate was **about 28.5–41.9 billed hours total,
€10.7–15.7 at €0.3758/h** (using eight already-billed hours conservatively).
For the minor implementation restart, allowing ten hours already billed gives
**about 30.5–43.9 hours total, €11.5–16.5**, still below the €45 stop threshold.
Remaining meaning work is 105,163 tracked files, projecting 2.0–11.4 h from the
measured Superset/Zig rates. Callgraph work is 1,450,239 tracked-file units,
projecting 10.5–14.5 h from Elasticsearch's 5:23/7:47 probes and assumed 9k/18k
file denominators; the exact probe file counts were unavailable. An additional
eight-hour reserve covers queries, cloning and nonlinear costs. These are
planning assumptions, not measured future durations or a guarantee. Monitor
actual server lifetime against the €45 review threshold and €50 ceiling.

Every meaning/full phase writes `semantic-failures.json`: native failed-file
count, any names supplied in structured status, the complete semantic status,
and all raw failure/error log lines (including any filenames AFT prints). Missing
names are explicitly unavailable, never reconstructed. These fields also reach
the repository CSV. An AFT semantic status of `ready` with nonzero `files_failed`
remains an observation, not a claim that every file was embedded successfully.

Example on the dedicated server, from `/root/aft`:

```sh
systemd-run --unit=aft-index-matrix --collect \
  --property=WorkingDirectory=/root/aft --property=KillSignal=SIGINT \
  --property=TimeoutStopSec=60 \
  --setenv=ORT_DYLIB_PATH=/root/onnx/onnxruntime-linux-x64-1.24.4/lib/libonnxruntime.so \
  /usr/bin/python3 scripts/telemetry/index-matrix/matrix.py \
  --revision meaning-nine-v2.1 --results /root/aft/target/index-matrix/full-matrix -- \
  --aft-binary /root/aft-default --shape-binary /root/aft-shape \
  --aft-source-commit 0672e983c119ab0e24cb4492c47c8c54f0e4127e \
  --shape-source-commit c1efafc8eb15b81e972ca80a895887e0fe45317c \
  --build-profile ci-storm --cgroup-parent /sys/fs/cgroup --drop-caches \
  --timeout-s 86400
```

`matrix-plan.json` and `matrix-state.json` preserve earlier plans/states and
record which phases each project requested and actually ran. Logs stay on local
disk; SSH disconnects do not terminate the service. Follow progress with:

```sh
systemctl status aft-index-matrix --no-pager
journalctl -u aft-index-matrix --no-pager
python3 -m json.tool target/index-matrix/full-matrix/matrix-state.json
# Per-project phase heartbeats and raw AFT logs:
tail -f target/index-matrix/full-matrix/ml-explore--mlx/run.log
df -h /
```

Stop safely with `systemctl stop aft-index-matrix` (SIGINT permits raw-result
cleanup), then reissue the launch command to resume. Completed repositories with
identical inputs are skipped. Unfinished attempts remain on disk as evidence;
a retry builds from empty storage and fresh model/HOME/XDG folders in a new
attempt directory, rather than continuing a partially built index.
Do not update runner/driver files or binaries during a running series. Changed
inputs require a new results folder. The driver pauses instead of deleting data
if free disk falls below 20 GiB before a project or 10 GiB during one; inspect
`paused_reason`, free space deliberately, then resume.

Keep keys only on the local collector. From the worker worktree, pull each
finished repository (success or failure), excluding checkouts, models and binary
stores but retaining raw JSON/CSV/status/query responses and AFT logs:

```sh
python3 scripts/telemetry/index-matrix/collect.py \
  --server root@2.29.202.177 \
  --identity target/hetzner-measure/id_ed25519 \
  --known-hosts target/hetzner-measure/known_hosts \
  --remote-results /root/aft/target/index-matrix/full-matrix \
  --output .cortexkit/internal-docs/investigations/index-matrix/full-matrix \
  --watch
```

Without `--watch`, collection runs once. `--stop-after N` hands off after copying
N distinct finished projects. Subsequent collectors use `collection.json` to
avoid repeating unchanged copies. A separate collector is required after worker
handoff; no credentials or reverse-SSH connection are installed on the server.
