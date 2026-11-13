# Fixed-memory index measurement matrix

This sibling of `../cost-gate.py` measures cold **text** and **meaning-search**
indexes separately, then optionally today's full graph and query latency. It
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

Run from the AFT repository root. `--repository` selects an exact manifest name;
omit it to run all 21 serially. A results-directory lock also prevents separate
invocations from overlapping. Keep results and checkouts under the ignored
`target/index-matrix/` folder (the default), **never commit them**.

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
  --repository neovim/neovim \
  --aft-binary target/aft-default --shape-binary target/aft-shape \
  --aft-source-commit "$(git rev-parse HEAD)" \
  --shape-source-commit "$(git rev-parse HEAD)" \
  --cgroup-parent /sys/fs/cgroup

# Entire matrix, same arguments but no --repository:
python3 scripts/telemetry/index-matrix/run.py \
  --aft-binary target/aft-default --shape-binary target/aft-shape \
  --aft-source-commit "$(git rev-parse HEAD)" \
  --shape-source-commit "$(git rev-parse HEAD)" \
  --cgroup-parent /sys/fs/cgroup

python3 scripts/telemetry/index-matrix/tables.py
python3 -m unittest discover -s scripts/telemetry/index-matrix -p 'test_*.py' -v
```

Reissue the same command to resume. A completed result is skipped only when its
manifest entry, binary/runtime/harness hashes, commits and measurement parameters
match. Changed inputs require a new `--results` directory. Incomplete attempts
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
`AFT_CALLGRAPH_SHAPE_CENSUS_JSON` only after a successful full build. The harness
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
