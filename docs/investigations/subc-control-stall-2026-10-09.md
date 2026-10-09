# Channel-0 frame-loop stalls, 2026-10-09

## Conclusion and delivery boundary

The two incidents are consistent with a **synchronous RouteBind preparation
wait**, but the logs do not identify the waited-on lock. An ordinary held
Mutating writer does **not** reproduce the stall: the new two-bind integration
test passes on the investigated baseline as well as with these diagnostics.
There is no demonstrated, contained root-cause repair in this delivery. Route
admission, executor scheduling, cancellation, and reply ordering are unchanged.

The highest-priority lock suspect is the executor scheduler mutex: bind
preparation takes it, and some of its holders enter lifecycle/component locks
or filesystem metadata operations. The bash live-session mutex is another
instrumented candidate, but its three critical sections do not contain disk,
SQLite, retention, kill, or child-process work. Slow config reads or root
canonicalization remain possible. These are ranked leads, not a recovered stack.

This investigation was reported to the task-giver **before product edits**.
The approved scope was diagnostics first, then reproduce, and repair only a
contained wait established by evidence. The task-giver additionally requested
holder acquisition site/time records for the scheduler and live-session locks.

## Evidence from the live daemon

Sources checked directly (outside the checkout):

- `~/.local/share/cortexkit/run/logs/aft.stderr.log:101337-101347`: routes 2400
  and 2401 log both `route bind` and `attach` at 16:05:57.092/16:05:57.099Z.
  No corresponding `bound to root` line appears before detection at
  16:06:12.517Z and exit at 16:06:27.596Z, `corr=3262`.
- The same log, `105401-105421`: Interactive apply_patch, edit, and bash jobs
  report Mutating execution ages of 60,197, 60,060, and 60,124 ms. Routes 124
  and 125 log `attach` at 16:11:10.812/16:11:10.817Z; detection follows at
  16:11:26.403Z, exit at 16:11:41.449Z, `corr=133`. Other calls exceed their
  25-second client deadline in both windows.
- Both named diagnostics files exist and have zero bytes:
  `stall-20261009T160612Z-955.txt` and
  `stall-20261009T161126Z-93072.txt`.

The watchdog's wake lateness is only 3–10 ms in those lines. Heavy machine load
can magnify a long critical section or disk operation, but these lines do not
show the entire process being descheduled for 30 seconds. Correlation IDs are
not decoded operation names. The last attach messages strongly suggest a bind
handler, but do not prove that either final frame was a RouteBind.

## Inline channel-0 work

Source references below describe the delivered source; the investigation was
performed against `fa131ea23de507063d5717988ed9e0e4833c7090`.

| Input | Inline work and possible waits |
| --- | --- |
| Hello/HelloAck | Initial module handshake precedes the steady-state loop (`subc/mod.rs:4235-4329` at the baseline). It includes transport I/O, not executor job completion. |
| Ping | Builds Pong and awaits writer-queue admission (`subc/mod.rs:4736`). No root writer/read permit or child process. Writer congestion is a possible bounded transport wait. |
| Goodbye, channel 0 | Ends the loop gracefully (`subc/mod.rs:4754`). Connection teardown happens after loop exit. It does not wait for a root Mutating job in this frame arm. |
| Response/Error, channel 0 | Resolves an outstanding readiness correlator (`subc/mod.rs:4819`). The database-readiness worker does the longer readiness work; the response arm does not execute a tool. |
| Request: HealthCheck | Assembles a cached reply and queues it (`subc/mod.rs:6246`, `6280`). Rollup reads use try-read and executor health probes use nonblocking snapshots (`subc/health.rs:1513-1624`, `1814-1885` at the baseline). A congested writer can delay the send; this is not a census waiting on Mutating work. |
| Request: management RouteBind | Validates identity/trust and installs immediately. A higher epoch first tears down the old route (`subc/mod.rs:6280`). There is no configure job for management routes. |
| Request: project RouteBind | Validates, canonicalizes the root, reads local config, registers/reuses an actor, synchronizes live bash sessions, then submits configure. These preparation steps are synchronous (`subc/mod.rs:6524`, `6569`, `6636`, `6659`). Configure completion is deferred. |
| Invalid/unsupported frames | Decode/validation and, where applicable, a refusal through the writer. They do not execute Mutating work. |

Route Goodbye is **not** a channel-0 close operation. It calls
`teardown_installed_route` inline (`subc/mod.rs:3257`, `4758`). A higher-epoch
channel-0 bind also calls that same teardown. Subscription/request settlement
may queue terminal frames; root cancellation/quiescence can enter executor,
registry, and lifecycle locks. This remains a separate synchronous boundary,
not a root-cause repair made here.

### What a new/root-busy bind actually waits for

1. Canonical root identity and local config involve filesystem work before
   configure submission (`subc/mod.rs:6524-6569`).
2. `register_actor_for_bind` checks the executor actor table and constructs and
   registers a context for a missing actor (`subc/mod.rs:5821`;
   `executor/mod.rs:1123`, `1321`). Registration also performs context/cache
   bookkeeping after releasing the actor-table lock.
3. `sync_bg_live_delivery_sessions` enumerates **all installed route roots**,
   not just the bind root. For each root it takes the scheduler mutex to clone
   the actor context and the bash live-session mutex to replace its set
   (`subc/mod.rs:3106`; `executor/mod.rs:1327`;
   `bash_background/registry.rs:1183`). Thus an unrelated contended root can
   delay this bind before its pending-bind deadline even starts.
4. `submit_bind_cancellable_async` queues a repeatable configure. The handler
   creates PendingBind and a task awaits its response; completion returns via
   the existing control-completion channel (`subc/mod.rs:6659-6707`).
5. Configure uses a shared epoch hold beside readers/maintenance when safe. If
   it needs to change the root, it requests an exclusive rerun. Admission
   behind a Mutating writer, reader hold, actor cap, or worker shortage delays
   **that deferred job**, not the loop by design (`executor/mod.rs:1519-1548`,
   `3974-4115` at the baseline). The epoch gate is acquired on workers, not
   under the scheduler mutex on the frame thread. Disk work and any child
   processes started by configure likewise belong to its worker.
6. Bind completion installs identity/lifecycle state and queues the ack; it
   can itself enter component locks (`subc/mod.rs:5952`). A no-bound-line log
   sequence alone cannot distinguish preparation from deferred configure
   delay. The watchdog's `phase=handle_frame` is the additional clue here.

Standing ticks are already handed to their own worker. The prior recursive
SubcLifecycleAdmission defect was repaired in `441340ebcf2544a66002982738449a12c1b85424`
(see `standing-lifecycle-stall-2026-10-06.md:40-89,172-207`). That history is not
evidence that this new incident is the same recursion.

### Lock-holder audit

- Bash live-session acquisitions are `record_live_delivery_session`,
  `replace_live_delivery_sessions`, and `originating_session_has_live_route`
  (`bash_background/registry.rs:1176-1205`). They only insert, replace, or
  check membership. Retention can call the membership check while holding
  SQLite/task state, but releases the live-session guard before further work
  (`registry.rs:3595-3618`, `3692-3701` at the baseline). There is no discovered
  multi-second I/O hold of this particular mutex.
- Scheduler maintenance admission holds state while querying lifecycle and
  `is_dir` (`executor/mod.rs:1809-1824`). Root-maintenance cancellation nests
  publication-registry/token cancellation (`executor/mod.rs:1273`).
- The scheduler event batch holds state through completion and admission
  (`executor/mod.rs:3173`). Requeue completion may query cancellation/lifecycle,
  perform throttled root metadata checks, or lock configure timing
  (`executor/mod.rs:3254`; baseline `3312-3337`, `685-753`). The run channel
  is unbounded; sending a job is not executing its child process under state.

These nested waits make scheduler contention a stronger lead than the small
bash membership sections. There is still no evidence identifying a particular
holder during either live incident.

## Empty captures and hardened-runtime-compatible evidence

The old macOS implementation created the capture file **before** spawning
`/usr/bin/sample`, discarded stderr, and reaped children without examining
status (`subc/stall_watchdog.rs:98-110,301` at the baseline). An attach/spawn
failure or interrupted sample could therefore leave a zero-byte file whose
path was nevertheless logged as a capture. The old incidents' stderr/status
cannot be recovered; the specific failure reason is unknown.

The current installed `ck-aft` has `flags=0x10000(runtime)`, is team-signed, and
has only `com.apple.security.cs.disable-library-validation` in its displayed
entitlements (no get-task-allow). A read-only probe of current PID 32231 using
the watchdog's exact command succeeded:

```text
/usr/bin/sample 32231 3 -mayDie -file /tmp/aft-stall-exact-probe-bg_754b3378.txt
exit=0, file size=965963 bytes
Sampling process 32231 for 3 seconds with 1 millisecond of run time between samples
Sampling completed, processing symbols...
Sample analysis of process 32231 written to file /tmp/aft-stall-exact-probe-bg_754b3378.txt
```

An earlier one-second probe also succeeded (1,043,671 bytes). This does **not**
prove that sampling the old processes, or launching sample from their precise
runtime environment, would have succeeded. It does mean hardened signing alone
is not an established explanation. No entitlement or production signing change
is made.

### Delivered diagnostics

- Frame context names the decoded control operation, bind root/session, last
  instrumented wait boundary, and (for per-root session synchronization) the
  actual awaited root (`subc/health.rs:755-870`, `subc/mod.rs:3106`). Both
  detected and terminal watchdog lines include context.
- Scheduler acquisitions, including try/timed acquisitions, publish a static
  source file/line and acquisition timestamp via an RAII wrapper
  (`lock_diagnostics.rs:1-118`; `executor/mod.rs:1046`). All three bash
  live-session acquisition sites record the same evidence. Diagnostic reads
  use atomics; they never take the observed mutex. Frame-operation publication
  and reads use a separate try-lock and explicitly report unavailable state
  rather than waiting.
- The watchdog writes a nonempty **in-process snapshot first**, which requires
  no permission to attach to the process. This works when an external profiler
  cannot attach. Profiler output goes to a private temporary side file and is
  appended only **after the child exits**. Exit status, stderr, and output size
  are recorded in the snapshot and logged; failure/empty output is explicitly
  labeled failed. Temporary output/stderr files are removed after collection
  (`subc/stall_watchdog.rs:396-532`). On watchdog shutdown a pending profiler
  is killed/reaped and its result collected.
- Ordinary `<storage>/diagnostics/stall-...txt` retention still applies to the
  combined evidence files. No real AFT storage was used for Rust tests.

`awaited` is the last instrumented code boundary, not a proof of which kernel
primitive is currently sleeping. `holder` is best-effort current mutex evidence,
not a historical ownership trace. A scheduler holder is also included independently
of the current frame boundary. Other lifecycle/component locks remain candidates;
these additions do not claim complete lock instrumentation.

## Reproduction and non-vacuity

`subc_bridge_back_to_back_binds_beside_held_mutating_job` starts a real Mutating
executor job, waits until it has the root, then sends two consecutive binds and
HealthCheck through the real fake-daemon transport. The executor has two workers
and actor cap one. The writer is released only after a health response, or a
bounded failed observation; receiving health proves the loop consumed both
preceding binds while the writer was held. Both deferred acks are then required.
It passed on baseline in 0.16 seconds: **this schedule does not reproduce the
live stall**. No synthetic CPU load or timing-only slow executor was used.

`blocked_route_bind_capture_names_operation_resource_and_scheduler_holder`
uses a separate real module loop and deliberately holds its scheduler mutex.
A real RouteBind then blocks at actor registration. The independent watchdog
must identify operation/root/session/resource/holder in the detected line,
terminal line, and in-process capture. This proves diagnostic coverage of a
real loop wait; it is not a claim that that injected holder caused the incidents.

Before the implementation, the operation-context test failed with exactly the
old generic context:

```text
subc::stall_watchdog::tests::frame_loop_context_names_control_operation_root_and_session ... FAILED
phase=handle_frame frame_type=0 channel=0 epoch=0 corr=133
```

Three staged-state mutation controls were run and safely restored:

1. Neutralize the operation label: only
   `blocked_route_bind_capture_names_operation_resource_and_scheduler_holder`
   failed; 16 other watchdog tests passed. The duplicate context unit assertion
   was excluded from this control so the real blocked-loop assertion was isolated.
2. Neutralize holder timestamp publication: only that same real blocked-loop
   test failed (`holder=none`); 15 other watchdog tests passed. Two duplicate
   holder/context unit assertions were excluded from this control.
3. Restore silent discard of completed profiler status/stderr: only
   `failed_profiler_records_status_stderr_and_preserves_in_process_snapshot`
   failed; 17 other watchdog tests passed. Its simulated profiler exits 7 with
   `task_for_pid denied`, and the test requires that failure in both the
   evidence file and log.

For each control, the live files were staged first, the mutation produced a
nonempty local `git diff --stat`, and `git checkout -- <mutated path>` plus
`touch` restored an empty unstaged diff. No mutation is retained.

## Next root-cause action

On the next incident, use `op`, `awaited`, `awaited_root`, `holder`,
`held_for_ms`, `scheduler_holder`, and the profiler-result line to choose the
actual wait to repair. Do not infer an epoch writer wait from a long Mutating
job census alone.

A broader off-loop bind-preparation change would need pending/cancellation
state **before** registration or submission can wait, ordered session-set
updates, late-completion cleanup, and the same deadline/refusal semantics as
current deferred configure replies. Merely awaiting `spawn_blocking` from
`handle_control_request` would still hold the frame loop and is not a fix.
Those lifecycle changes are not justified by the current reproduction, so this
delivery deliberately contains the report, diagnostics, and tests only.

## Verification

All Rust gates ran on the remote Linux runner, one compile at a time, with a
fresh temporary HOME/XDG config/data location; Cargo/rustup caches were retained.
Tools: `cargo 1.99.0 (5f94df478 2026-08-27)`,
`rustc 1.99.0 (b940084d7 2026-09-28)`,
`rustfmt 1.10.0-stable (b940084d7e 2026-09-28)`.

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | Exit 0 (silent-success gate). |
| `cargo test -p agent-file-tools --lib -- subc::` | 339 passed, 7 ignored, 0 failed. |
| `cargo test -p agent-file-tools --lib -- lock_diagnostics::` | 1 passed, 0 failed. |
| `cargo test -p agent-file-tools --lib -- executor::` | 109 passed, 2 ignored, 0 failed. |
| `cargo test -p agent-file-tools --bin aft --` | 122 passed, 0 failed. |
| `cargo test -p agent-file-tools --test integration -- subc_` | 171 passed, 5 ignored, 0 failed. |
| `RUSTFLAGS="-D warnings -A deprecated" cargo check -p agent-file-tools --tests --target x86_64-pc-windows-gnu` | Finished dev profile; compile-only, not Windows runtime coverage. |
| Scoped AFT diagnostics | Authoritative diagnostics for 5/5 inspected files: 0 errors/warnings. Overall inspection remains PARTIAL because the checkout call-graph analysis is unavailable. |

The first broad remote integration run had 170 passes and one failure:
`subc_launch_nonce_test::subc_module_reads_the_pipe_nonce_and_no_child_inherits_it`,
`HELLO build Git SHA`. The synced runner did not supply Git-derived provenance
(`crates/aft/build.rs:22-40,73-75`). Setting the existing compile-time
`AFT_BUILD_GIT_SHA=fa131ea23de507063d5717988ed9e0e4833c7090` input rebuilt the binary
and made the complete subc integration gate pass, without a product change.
The stamp names the pre-commit worktree HEAD, not a claim of a clean release
artifact. No test expectation was inverted. Existing watchdog assertions were
adjusted to inspect the primary combined `.txt` snapshot rather than the new,
transient profiler side file.

The report and mutation summaries are committed source documentation, outside
regenerable build directories. No manifests, lockfiles, tool arguments, config
keys, search ranking, or search-engine ownership were changed.
