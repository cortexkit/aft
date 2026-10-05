# Pi standalone startup latency (#399)

## Diagnosis

The issue's timestamps do **not** mean that stdin waits inside the semantic
start gate. In v0.58.2, `commands/configure.rs:5799-5808` spawns
`aft-semantic-view`; both `wait_for_semantic_artifact_start` and
`semantic_runtime::run_worker` run on that thread. Model initialization,
semantic view loading and the 30-second grace are already off stdin. The same
is true on the investigated main revision, `52f09206136a11e1cd18ebf8348d58200bae6e55`
(`commands/configure.rs:5871-5939`, `views/semantic_runtime.rs:675-799`).

The release's configure tail still runs the **other** view publication inline.
`commands/configure.rs:6813-6836` calls `view_publication::schedule` from
`ViewLoad`; `executor/view_publication.rs:119-126` falls back to synchronous
`ctx.publish_view_paths` when there is no actor scope. Pi's standalone NDJSON
loop has no daemon actor. Its subsequent `Callgraph` and `SemanticRelease`
stages therefore cannot run until publication finishes.

Relevant reporter lines (2026-10-04):

* 17:45:21: `content-addressed view publication scheduled paths=274`.
* 17:45:51: `semantic artifact load proceeding without callgraph build_started after 30s`.
* 17:45:51.913: `tool_call ... timed out after 30000ms`.
* 17:46:04: `semantic view loaded ... load_ms=11435`.
* 17:46:06.918: `bash_drain_completions ... timed out after 15000ms — restarting bridge`.
* In the surviving later process, 18:28:35: `view_publication ... blobs_ms=210291 ... total_ms=212913`,
  immediately followed by `configure maintenance yielded to 1 queued request(s)`.

The semantic worker's grace expiry is a **symptom of the tail not reaching its
release stage**, not the request thread's blocking stack. The later view
publication completion explains the minutes-long request-loop stall. The
2.6-second retention mutation lock is another delay visible in the report, but
neither its budget nor storage durability is changed here.

Train 274's `7301d5602` detached standalone publications by installing a
standalone actor scope. It addressed that release-time publication stall;
there was no additional synchronous semantic load to move. Main still opened
view stores, read HEAD/manifest/aliases, marked pending paths, performed legacy
migration, and swept view storage in individual synchronous configure-tail
units. Yielding *between* units does not protect a request arriving *inside* a
slow unit. The slow-I/O regression below reproduces that remaining problem.

There is also a distinction between the legacy callgraph builder's
`build_started` event and graph extraction owned by a view publication. A
pending view does not imply that a legacy callgraph receiver exists. The
existing `should_wait_for_callgraph_start` requires both `Building` and that
receiver; this change does not remove the legacy event-ordering contract. The
similar bound-checkout case named in the brief may share delayed configure-tail
admission or confusion between these two graph owners. Its actual record/logs
were not available in this worktree, so a shared underlying bug is not proven
and that case is not fixed here.

## Change

* Standalone view opening/migration prepares a snapshot on `aft-view-open`.
  Stdin polls a result channel without waiting, then installs only the current
  configure generation's result and schedules the already-detached publication.
  Inputs are captured before spawning, so reconfigure cannot make an old
  worker read a new root's semantic producer/config. Superseded results are
  discarded. The daemon's maintenance path keeps its existing synchronous API.
* View generation/blob sweeps, like standalone storage sweeps, run separately.
* A views-on Git checkout with no installed graph reports `callgraph_building`
  instead of opening/building a second legacy graph during view startup.
  Non-Git legacy navigation keeps its old fallback.
* The bridge treats completion replay as a bounded, non-killing poll; Pi also
  explicitly requests that behavior at its 15-second delivery hop.
* The shared bridge permits one timeout-triggered replacement until a successful
  non-plumbing request proves recovery. Configure/version/status/replay do not
  re-arm it. A replacement that encounters the same startup cost is kept warm;
  each request still fails at its original deadline. Its agent-facing error says
  the automatic restart was already attempted, advises a later retry or explicit
  host restart, and warns that a timed-out mutation may still complete. Crash
  recovery remains separate. No timeout was lengthened.

The timeout kill decision is in `packages/aft-bridge/src/bridge.ts`'s `send`
timer (around lines 1043-1084), not in the Pi tool adapter. `handleTimeout`
(around line 1752) kills/rejects siblings and logs `Bridge killed after timeout`.
Previously that path did not consume the crash restart budget, and spawn reset
the per-process timeout counter, so the next call could repeat it indefinitely.

## Reproduction and measurements

Native test host: macOS; Rust 1.99.0, Cargo 1.99.0. The reporter's Linux x64,
four-core machine was not available. The portable regression spawns the actual
standalone binary with `harness: pi`, a committed 500-file Git fixture and views
on. `test-timing-hooks` adds phase-observed **storage-delay** seams, not CPU
stress, and uses a local deterministic HTTP embedder instead of a model download.

| Experiment | Result |
| --- | --- |
| Main before fix; view-open I/O delayed 3,000 ms | tool_call/read **3,203 ms**, completion drain 0 ms, status 55 ms; regression failed |
| Patched; same view-open delay | tool_call/read **1 ms**, completion drain 0 ms, status 3 ms |
| Patched; view sweep delayed 3,000 ms | completion drain **33 ms** |
| Patched; view open held 35 s, forcing the real 30 s semantic grace | 193 startup samples, maximum **31 ms** across read/drain/status |
| Patched; semantic disk load delayed 5,000 ms after the grace | 3 startup samples, maximum **1 ms** across read/drain/status |
| Existing detached publication test with a 20 s delay | slowest request **29 ms**; publication still committed |

The semantic regression checks `semantic_index.status == loading` during both
startup phases and eventually observes `ready`. The view-open regression also
checks that navigation discloses `callgraph_building` promptly, rather than
waiting for a legacy build.

Commands:

```
cargo test -p agent-file-tools --features test-timing-hooks --test integration standalone_ -- --nocapture
cargo test -p agent-file-tools --lib commands::configure::tests:: -- --test-threads=4
bun run --cwd packages/aft-bridge build
bun run --cwd packages/aft-bridge typecheck
bun run --cwd packages/pi-plugin typecheck
bun run --cwd packages/aft-bridge test:unit
bun run --cwd packages/pi-plugin test:unit
bun run lint
```

The first native run passed 30 standalone integration tests. Configure lib:
146 passed, 3 ignored. Final TypeScript runs: bridge 764 passed/3 skipped/0
failed; Pi 798 passed/0 failed. TypeScript 5.9.3 typechecks exited 0; Biome 2.4.7
checked 684 files. Broader final native checks are recorded in the delivery.

The required strict Windows check did **not** finish. A 30-minute attempt and
its 120-minute continuation repeatedly queued behind the machine's six shared
compile slots, then hit their command deadlines without a `Finished` line.
It is unverified, not passed. Per the reviewer's instruction, no further
Windows retry is run here; the parent will run:

```
RUSTFLAGS="-D warnings -A deprecated" cargo check -p agent-file-tools --tests --target x86_64-pc-windows-gnu
```

## Mutation controls

Each control staged the live files, confirmed an empty unstaged diff, introduced
a `NON-VACUITY BREAK`, captured a non-empty diff stat, ran its named regression,
then restored from the index and captured an empty unstaged diff. No mutation
remains in the delivery.

| Reverted control | Exact red test | Unaffected companion |
| --- | --- | --- |
| Route standalone tail back to synchronous view open | `per_checkout_9::standalone_view_open_never_blocks_requests` (3,258 ms read) | `standalone_view_publication_never_blocks_requests` |
| Run standalone view sweep inline | `per_checkout_9::standalone_view_open_never_blocks_requests` (2,915 ms drain) | `standalone_view_publication_never_blocks_requests` |
| Allow legacy graph startup before view installation | `per_checkout_9::standalone_view_open_never_blocks_requests` (expected `callgraph_building`, got a ready legacy graph) | `standalone_view_publication_never_blocks_requests` |
| Allow a superseded standalone load to install | `commands::configure::tests::superseded_standalone_view_load_never_installs_its_snapshot` | none selected |
| Neutralize timeout replacement recovery latch | `timeout respawn cannot repeat startup kills before a tool recovers` | `completion drain timeouts never escalate into a startup kill` |
| Remove bridge's non-killing drain classification | `completion drain timeouts never escalate into a startup kill` | `timeout respawn cannot repeat startup kills before a tool recovers` |
| Remove Pi's non-killing poll option | `completion replay uses a bounded non-killing transport poll` | `first no-task path force-drains once for replayed completions` |
