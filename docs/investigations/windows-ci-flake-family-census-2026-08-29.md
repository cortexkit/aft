# Windows CI migrating-flake family census — 2026-08-29

## Scope and evidence

The census queried failed attempt logs with `gh run view <id> --attempt 1 --log-failed` and enumerated earlier main failures with `gh run list --branch main --status failure`. Assertions below use the failing revision's logged line when the retained log named it. Four older specimen labels did not appear in the retained failed logs returned for the supplied run IDs or the main-failure window, so their assertion locations are taken from the source revision at the census base and are marked **source-derived** rather than attributed to an invented run.

Two supplied IDs contained failures outside the eight-specimen inventory:

- `33261404163` attempt 1: Linux `agent_child_env::tests::configure_maintenance_refreshes_stale_gh_links_and_removes_disabled_entries`, `agent_child_env.rs:797:35`.
- `33267801352` attempt 1: Windows `inspect_command_test::rust_quiescence_promotes_latest_publish_for_warm_inspect_and_status_bar`, `inspect_command_test.rs:2469:10`, plus a Pi RPC E2E failure.

The logs also show `artifact_owner::dead_owner_is_reclaimed_by_different_checkout` in both `33189547197` and `33246166305` attempt 1. That is a second repeated name in the queried window, in addition to the acknowledged inspect pair; it should not be hidden to preserve the “one name once” premise.

## Mechanism classes

- **S — scheduler time charged as work time.** A wall-clock budget starts before the operation has entered the lifecycle state being measured. A descheduled test/child or queued job can exhaust the budget without the operation blocking or violating its own contract.
- **P — completion proxy.** The fixture waits on a correlated-looking request, status sample, PID sentinel, or filesystem sample rather than the producer's terminal event and authoritative store.
- **G — unscoped global test control.** A process-global arm or environment value can be consumed by an unrelated parallel test/build. The assertion then observes the wrong lifecycle.
- **F — filesystem/PID observation race.** A path or liveness probe is sampled while deletion, rename, process exit, or an environment-routed namespace can still change underneath it.

## Census

| # | Specimen and exact failing assertion | What it waits on / slow-runner effect | Class | Event-based fix pattern | State |
|---|---|---|---|---|---|
| 1 | `fs_lock::try_acquire_once_never_waits_behind_live_owner`; **source-derived** `fs_lock.rs:1275`, `panic!("try-acquire blocked behind the live owner")` after `result_rx.recv_timeout(2s)` | The contender announces `started` before entering `try_acquire_once`. Descheduling between that send and the call is charged to the two-second “blocked” budget. | S | Inject/observe the retry decision itself: assert that zero-timeout live-owner acquisition emits terminal `Timeout` without entering the sleeper. Keep only a generous outer thread-join hang catch. | **Queued P1** |
| 2 | `subc_storm_detach_rebind_replays_completion_and_preserves_bash_task`; **source-derived** `subc_storm_test.rs:1803-1805`, `"reliable completion was not replayed after rebind"` | The fixture sends route goodbye, releases a deferred push, and immediately rebinds. It has not observed route-detached/retained-completion admission before starting the replay budget; slow transport scheduling can move those lifecycle transitions across the rebind. | P/S | Await the route-detached event and retained-completion registration, then rebind and await the matching replay event. Verify the background task through its task registry; retain one outer hang bound. | **Queued P1** |
| 3 | Callgraph pointer libtest arm; **source-derived base** `context.rs:8252-8255`, expected `callgraph_store_for_ops()` to remain `Building` after the pointer mutation | A global `AtomicBool` was consumed by whichever callgraph build reached inline reopen first. Parallel libtest completion could remove an unrelated pointer and leave the intended pointer published. | G | Key mutation arms by the authoritative pointer path. Consume an arm only when that exact generation pointer reaches inline reopen; cleanup removes only the same key. | **Fixed here** |
| 4 | Bash orchestration timing arm; **source-derived** `bash_orchestrate_test.rs:114-117`, `started.elapsed() < 4s`, `"promotion response took too long"` | The stopwatch starts before request transport and child scheduling, so runner descheduling is counted as foreground orchestration latency. The response/state transition can be correct after the wall-clock assertion has expired. | S | Configure the clamped five-second foreground window explicitly, hold the real child on a release sentinel, and assert the promotion response, authoritative `running` status, and terminal completion frame. Keep only a twelve-second transport hang catch. | **Fixed in P2 follow-up** |
| 5 | `artifact_owner::dead_owner_is_reclaimed_by_different_checkout`; runs `33189547197`, `33246166305`; logged `artifact_owner.rs:879:10`, `.unwrap()` received Windows `Os { code: 3, kind: NotFound, "The system cannot find the path specified." }` | The fixture writes a synthetic owner with PID 0, then treats that sentinel as proof of a completed owner lifecycle. Windows liveness/path namespace behavior and process-global storage routing can change between manifest creation, reclaim, and replacement. | P/F | Spawn a real short-lived owner process, wait for its exit event, re-read the unchanged manifest from the resolved storage root, then reclaim with compare-and-delete and verify the replacement manifest from that same root. | **Queued P1** |
| 6 | Inspect stat-verify arm; run `33240358872`; `inspect_tsconfig_membership_test.rs:303:5` logged `"metrics could not be stat-verified"`; sibling at `:347:5` logged `"metrics did not complete"` | A warm-LSP action is used as readiness evidence while inspect independently queues Tier-2 and Tier-1 work and takes before/after filesystem samples. Slow queueing can exhaust Tier-1's soft deadline; file appearance/disappearance between walk and stat produces a separate terminal. | P/S/F | Wait for the producer's authoritative report event, run the requested category, and verify the published report/cache. Exercise stat rejection with the existing body/stat gate and a deterministic mutation, not incidental LSP timing. | **Queued P1** (Tier-1 self-starvation covered by fix #8) |
| 7 | `blocking_inspect_waits_for_a_warming_producer_to_settle` and `blocking_inspect_is_fresh_when_producers_quiesce_without_reports`; runs `33256217428`, `33258657999`; both logged `inspect_command_test.rs:347:5`, `"metrics did not complete"` | The shared fixture launched serial Tier-2 work, then used an unrelated inspect request as its completion proxy. On a slow one-thread pool, Tier-2 remained healthy while metrics lost its one-second soft deadline. | P/S | Wait for the manager's in-flight lifecycle to end, then read every requested category through the freshness-checking authoritative cache. | **Fixed previously** by `3a98522a8` / main equivalent `3385126da` |
| 8 | `inspect_tier2_scheduler_test::linked_worktree_explicit_inspect_keeps_parent_aggregates_byte_identical`; run `33273890422`; logged `inspect_tier2_scheduler_test.rs:575:5`, `"worktree inspect failed"`, payload `"metrics did not complete"` | The nonblocking inspect path queued all parse-heavy Tier-2 jobs before joining Metrics and Todos. With one inspect worker, its own Tier-2 work consumed the Tier-1 soft deadline. | S | Join the real Tier-1 completion events before enqueuing Tier-2 on the nonblocking path. The soft bound remains a scan hang-catch, no longer a queue-position lottery. Verify Tier-2 through its own terminal events/cache. | **Fixed here** |

## Delivered fixes and controls

### Scoped callgraph pointer arm

The pointer-removal mutation seam is now a set keyed by the full authoritative `.current` pointer path. An unrelated callgraph completion cannot consume or clear another test's arm. The regression presents an unrelated pointer event before the target event. Mutation control changed the consumer back to “take any arm”; the regression failed on the unrelated pointer assertion, then passed after restoration.

### Tier-1 before Tier-2 on nonblocking inspect

The nonblocking inspect path now joins Metrics and Todos before it launches Tier-2 work. This does not widen the one-second soft deadline. It removes self-generated queueing from that deadline while the blocking path retains its existing hard phase budgets and concurrent Tier-2 behavior.

Deterministic contention used one inspect worker plus the existing 300 ms Tier-2 delay seam:

```text
AFT_INSPECT_POOL_THREADS=1 AFT_TEST_TIER2_REUSE_DELAY_MS=300 \
  cargo test --features test-hooks -p agent-file-tools --test integration \
  inspect_tier2_scheduler_test::linked_worktree_explicit_inspect_keeps_parent_aggregates_byte_identical \
  -- --exact --nocapture
```

Before the fix it failed in 1.96 s at the parent assertion with `metrics did not complete`. After the fix it passed in 3.62 s. This is queue contention, not sleep-and-hope, and directly models rule 6987's slower-runner regime without widening a budget.

### Bash orchestration wait-window arms

The class column records the pre-follow-up exposure. A dash means the arm has no census mechanism; every arm still crosses the binary protocol and some intentionally spawn a real subprocess.

| Arm in `bash_orchestrate_test.rs` | Class | Disposition |
|---|---|---|
| `orchestrated_fast_foreground_returns_single_terminal_response` | S | Hardened. The old 600 ms process environment made child scheduling decide whether the response was terminal. The arm now configures a generous twelve-second positive window and asserts the terminal protocol response. |
| `orchestrated_non_zero_exit_appends_exit_code_marker` | S | Hardened for the same short-window exposure; the terminal response remains the authoritative exit/output event. |
| `orchestrated_foreground_promotes_after_wait_window` | S | Rewritten. A configured five-second window and sentinel-held child force the promotion decision; the response, registry `running` status, and matching `bash_completed` frame replace elapsed-time bounds. |
| `orchestrated_block_to_completion_does_not_promote` | — | The terminal response already proves the blocking decision. The redundant lower elapsed bound was deleted; the request's three-second timeout is the hang budget. |
| `orchestrated_wait_true_does_not_promote` | — | The late output and terminal response prove wait-mode completion. The redundant lower elapsed bound was deleted; the explicit request timeout bounds the send. |
| `orchestrated_wait_true_honors_short_timeout` | — | Left as-is: the product timeout and terminal `timed_out` response are the behavior under test, not an unrelated fixture stopwatch. |
| `orchestrated_wait_rejects_background_and_pty` | — | Left as-is: validation returns before process launch and has no wait or completion proxy. |
| `orchestrated_background_returns_formatted_launch_immediately` | — | Left as-is: it asserts the launch decision frame, not a latency bound or child completion sample. |
| `bash_gate_off_still_returns_spawn_response` | — | Left as-is: it pins the immediate spawn-response schema and does not sample process completion. |
| `pending_orchestrated_bash_does_not_starve_push_frames` | — | Left as-is after control with an immediately completing child: it still passed because it asserts protocol-frame ordering, filters unrelated push frames, and uses twelve-second reads only as hang catches. |
| `abort_inflight_kills_wait_registered_foreground_and_settles_deferred_response` | S/P/F | Rewritten. A release sentinel keeps the admitted child open until abort; protocol responses prove abort and deferred settlement, registry status proves `call_aborted`, and the remaining direct PID-death check has a twelve-second hang catch. |

Red-first controls forced the old failure interleavings rather than waiting for host load: a one-second child crossed the old 600 ms window in each fast terminal arm; a four-second pre-send delay tripped the promotion arm's old four-second stopwatch; and a four-second pre-marker child delay tripped abort admission's old three-second budget. The failures were respectively `running` instead of `completed`, missing exit code 3, `promotion response took too long`, and `foreground task did not start`.

The decision guards were also mutation-checked. Configuring promotion to 20 seconds made its twelve-second transport hang catch fail instead of accepting an early promotion. Routing abort through a different bound session produced `killed: 0` and no deferred settlement, so the registered-task and settlement assertions both remained non-vacuous. All `NON-VACUITY BREAK` mutations were restored before the passing run.

## Verification notes

- The full Rust gate's unit phase passed all 2,851 unit tests. Its integration phase then hit five pre-existing `callgraph_test` failures because those fixtures configure paths inside this Mason linked checkout; artifact ownership correctly makes the checkout read-only, while those legacy tests require a writer. Running the integration gate excluding that baseline family passed all 1,724 selected tests, and the separate watcher suite passed all 16 tests.
- `cargo check --workspace --all-targets --target x86_64-pc-windows-gnu` passed.
- Native Ally verification was attempted at both `ufuka@192.168.1.33` and `ufuka@asusallyko.local`. The address timed out and mDNS did not resolve, so the Ally was not reached.
- The 2026-08-30 P2 follow-up ran all 11 orchestration arms successfully. The full cargo gate completed 2,890 library tests but hit two unrelated parallel-only failures (`concurrent_cold_callgraph_store_for_ops_spawns_one_build` and `response_body_read_failures_are_marked_transient`); each passed immediately in isolation. The integration suite hit the linked-worktree `callgraph_unavailable` baseline, while the same suite excluding `callgraph_test` passed all 1,584 selected arms and the serialized watcher suite passed all 18. The Windows GNU all-target check also passed.

## Ranked follow-up briefs

1. **P1 — artifact-owner lifecycle fixture:** replace PID 0 with a child-exit event and pin manifest resolution to one authoritative storage root.
2. **P1 — filesystem lock non-wait proof:** add a retry/sleeper observer so zero-timeout behavior is proven without scheduler wall time.
3. **P1 — inspect stat fixture:** publish/wait for the real producer report and use the existing inspect body/stat gate for deterministic mutation coverage.
4. **P1 — detach/rebind replay:** expose/await route-detached and retained-completion admission events before rebind.
5. **Delivered P2 — bash promotion timing:** the follow-up now configures the foreground window, gates child completion, and asserts promotion, registry, and completion protocol events without a latency claim.
