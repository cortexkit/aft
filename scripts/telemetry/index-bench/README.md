# Native cold-index comparison

`index-bench.yml` compares the official AFT 0.65.0 binary with the pushed candidate
on the self-hosted Windows and macOS runners. It does not launch a job VM or use
the Mac mini. Nothing runs on a normal branch push.

From a reviewed candidate commit, push **one** of these branches:

```sh
git push origin HEAD:refs/heads/bench/index-windows
git push origin HEAD:refs/heads/bench/index-mac
```

The workflow's `cold-index-<platform>-<run id>-<attempt>` artifact contains
`results.csv`, `results.md`, binary identities, the exact manifest pins and raw
per-run protocol/stderr logs. Each completed or skipped row is saved immediately;
partial results are uploaded even if a later step fails. Clones, isolated stores,
downloads and the candidate's build folder are deleted even on failure. The
candidate executable is copied out and its build folder deleted before cloning;
projects are cloned and removed one at a time to limit disk use on the rented Mac.

## Measurement contract

- One fresh store per binary/project, with text and call graph enabled and meaning
  search disabled. Readiness uses the Linux matrix's configuration and status
  helpers. Both the text index and call graph must report `ready`.
- “Cold” means no prior AFT store, not a flushed operating-system page cache.
  Filesystem caches can be warm. Versions are interleaved within each project,
  and their starting order alternates between projects. Chromium is always last.
- All repository commits come from `../index-matrix/manifest.json`. Clones fetch
  only the pinned commit at depth one, without submodules. Windows uses non-cone
  sparse checkout to omit paths Windows cannot represent; the exact excluded
  tracked paths and count are preserved with each project.
- macOS's exact AFT child peak and CPU time come from `wait4` (`ru_maxrss` is
  bytes on Darwin). Linux's proof uses the same path with the KiB conversion.
  Windows uses `GetProcessMemoryInfo` peak working set and peak pagefile-backed
  commit, and `GetProcessTimes` kernel plus user time. No helper's memory or CPU
  is included. Wall time ends when readiness is observed, not after teardown.
- The guard samples every 250 ms (macOS reads current RSS via `ps`; the final peak
  still comes from `wait4`). It kills at 13 GiB RSS on macOS or at **either**
  26 GiB peak working set or peak commit on Windows, recording the tripping
  counter(s) and final peak. Polling can overshoot; these are not kernel limits.
- AFT 0.65.0 runs only on Kubernetes/Linux on the Mac, and VS Code/Kubernetes/
  Linux/Chromium on Windows. Other baseline rows say `skipped: exceeds machine`.
  The candidate runs on every included project. Chromium is not cloned on macOS.
- Per-run deadlines are 45 minutes, or 90 minutes for Chromium. A deadline is
  `killed_by_timeout`, not a ready result. The workflow allows eight hours to
  leave room for the Windows Chromium pair, building, cloning and cleanup.
- Store size is the sum of file lengths after the child exits (not allocated
  filesystem blocks). CPU/peak include the short teardown; indexing wall does
  not. A ready child is killed only after its ready status has been saved.

## Small Linux proof

The same driver supports a Neovim-only proof; it is not a third workflow target:

```sh
python3 scripts/telemetry/index-bench/run.py download --work target/native-bench-work
cargo build -p agent-file-tools --release --features wasm-grammars
python3 scripts/telemetry/index-bench/run.py run --projects neovim \
  --baseline target/native-bench-work/bin/aft-0.65.0 \
  --candidate target/release/aft \
  --work target/native-bench-work --output target/native-bench-results
python3 scripts/telemetry/index-bench/run.py cleanup --work target/native-bench-work
```

The download command verifies the release executable against the release's
SHA-256 checksum file before executing it. The run command records each
binary's own SHA-256 and `--version` output, the host, guard and timeout policy.
It fails after saving results if any attempted build did not reach readiness;
guarded and timed-out runs remain usable measurements, not silently green runs.

## Profiling

`run --profile` adds, per measured version folder:

- `timeline.csv`: one row per second with RSS, process CPU seconds and (Linux)
  the thread count. The CPU slope over a stretch is its average parallelism.
- `profile.json`: each moment a string or boolean field of the
  `search_index` or `callgraph_store` status changes, in seconds since AFT
  started, and the record of each stack sample.
- `samples/sample-NNNNs.txt.gz` (macOS only): `sample` output for a 3 s window
  every 15 s, per thread. `download --dsym` places the release dSYM next to the
  0.65.0 binary, and the workflow copies the candidate's dSYM next to it, so
  `sample` names frames in both stripped binaries; `atos` resolves any frame it
  leaves unnamed. Both versions pay the same sampling cost.

`--rust-log` replaces the measured process's `RUST_LOG=info`. Every
`measurement.json` also carries the start time (Unix seconds), to align log
lines, and the child's `rusage` counters (context switches, block I/O, faults).
With `--profile`, Linux runs accept any project, so one project can be compared
across platforms with the same instrumentation.

Offline harness checks:

```sh
python3 -m unittest discover -s scripts/telemetry/index-bench -p 'test_*.py' -v
```
