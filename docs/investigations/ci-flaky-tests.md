# CI test flakiness investigation

## Evidence window and counting

Collected on 2026-10-07 from `cortexkit/aft`, workflow `tests.yml`, for
2026-09-16 through 2026-10-07 (UTC). `gh run list --created '>=2026-09-16'
--limit 1000` returned **657 main/train runs**. Failed-attempt logs were read
with `gh run view --attempt N --log-failed`; passing rerun job logs were read
with `gh run view --attempt N --json jobs` and `gh run view --job ID --log`.
The latter is important: a successful workflow is not evidence that a
nonblocking Windows test passed, or that a cancelled test actually ran.

The table counts **distinct (run, attempt, platform/job, test) failing events
with a subsequently executed PASS at the identical SHA**, not the repeated
failure printed in a nextest summary. Last seen is the failing run's creation
time, in UTC. Counts are confirmed recurrence counts, not total failures.
Links identify failing attempts; the final column names the passing attempt.
Same-SHA evidence establishes nondeterminism, not necessarily a timing cause.
Assertions exposing a product race must not be weakened to hide that race.

Twelve log downloads failed with transient HTTP/stream errors. Consequently
this is a lower bound, not a claim that missing logs contained no flakes.
Full-run `--log` downloads hit gh's missing-job-log request limit; narrowing
to named jobs recovered passing test evidence. Next-run candidates are not promoted to confirmed flakes merely because their
test file was unchanged. The initial correlation found 43 such failure/pass
pairs (25 names); none of the different-SHA pairs also had unchanged Rust/TS
production dependencies. Same-SHA test execution is the evidence used here.

## Confirmed same-SHA flakes

| Test | Platform | Count | Last seen (UTC) | Failure text / cause class | Failing run → passing attempt |
| --- | --- | ---: | --- | --- | --- |
| `Pi enabled config toggle > disabled config registers nothing without resolving a binary or creating a bridge pool` | macOS | 1 | 09-22 14:32 | Expected disabled log message; received empty string after fixed 600 ms sleep. Log flush timing. | [35741038342](https://github.com/cortexkit/aft/actions/runs/35741038342/attempts/1) → 2 |
| `bash tool adapter > transport-dead fallback recovers on the next successful module call` (Pi) | macOS | 1 | 09-21 00:36 | `timed out after 20000ms`. Child/host fallback liveness. | [35548243286](https://github.com/cortexkit/aft/actions/runs/35548243286/attempts/1) → 2 |
| `bash_background_persistence_test::bash_status_legacy_persisted_task_is_quarantined_on_replay` | macOS | 1 | 09-18 23:51 | `replay left the legacy metadata file in place, but a direct quarantine of it succeeds`. Persistence ordering/race. | [35407268324](https://github.com/cortexkit/aft/actions/runs/35407268324/attempts/1) → 2 |
| `bash_pty_test::pty_windows_wrapper_script_runs_utf8_command` | Windows | 1 | 10-04 18:37 | Output contained only terminal negotiation/title escape sequences. PTY readiness. | [37225148728](https://github.com/cortexkit/aft/actions/runs/37225148728/attempts/1) → 2 |
| `bash_pty_test::pty_write_python_repl_round_trip` | Windows | 1 | 09-19 04:02 | `timed out waiting for "pty-repl-ok"`; last output ended at `>>> `. PTY readiness/input. | [35420238745](https://github.com/cortexkit/aft/actions/runs/35420238745/attempts/1) → 2 |
| `bash_token_count_test::bash_completed_frame_compressed_tokens_reflect_compression` | macOS | 3 | 09-30 21:38 | `expected compressed token count 613 to be lower than original 613`. Output/compression snapshot race, not a latency assertion. | [36097850252](https://github.com/cortexkit/aft/actions/runs/36097850252/attempts/1), [36706127153](https://github.com/cortexkit/aft/actions/runs/36706127153/attempts/1), [36780704669](https://github.com/cortexkit/aft/actions/runs/36780704669/attempts/1) → 2 |
| `callgraph_test::callgraph_rust_use_imported_short_callers_resolve` | Linux | 1 | 10-03 20:16 | `timed out after 60s waiting for response line from aft stdout (child still running)`. Product/maintenance liveness. | [37150941148](https://github.com/cortexkit/aft/actions/runs/37150941148/attempts/1) → 2 |
| `doctor --issue body integration > tool failures section is included in assembled body and survives cap helper` | macOS | 1 | 10-03 05:11 | `timed out after 5000ms`. Cold imports/default Bun timeout. | [37098997194](https://github.com/cortexkit/aft/actions/runs/37098997194/attempts/1) → 2 |
| `e2e semantic search tool > aft_search degrades to a lexical fallback when semantic is disabled` | Linux, opencode-ndjson | 1 | 09-29 01:38 | Expected `src/lib.rs:1 [lexical match]`; got `Found 0 results.\nSearch status: partial/incomplete.`. Cold lexical readiness/coverage. | [36508835945](https://github.com/cortexkit/aft/actions/runs/36508835945/attempts/1) → 2 |
| `effective_path_cache_test::valid_cache_skips_sleeping_shell_and_returns_ping_quickly` | Linux | 1 | 09-24 14:24 | `served PATH "" does not include cached entry /cached/login/bin`. Child fixture/startup ordering. | [36012527382](https://github.com/cortexkit/aft/actions/runs/36012527382/attempts/1) → 2 |
| `inspect_command_test::scoped_rust_inspect_drops_a_fixed_compiler_error_with_real_rust_analyzer` | Linux | 1 | 10-01 00:26 | `control: the first check reports the error` failed. Real-server diagnostics generation/readiness. | [36796192789](https://github.com/cortexkit/aft/actions/runs/36796192789/attempts/1) → 2 |
| `inspect_command_test::scoped_rust_inspect_reports_a_removed_field_after_an_outside_edit_with_real_rust_analyzer` | Linux | 1 | 10-05 15:13 | `rust-analyzer's own error for the removed field is missing`. Real-server edit/check ordering. | [37331104560](https://github.com/cortexkit/aft/actions/runs/37331104560/attempts/1) → 2 |
| `inspect_command_test::unscoped_inspect_names_an_unfinished_cargo_check` | Linux, Windows | 2 | 10-01 17:41 | Expected one Rust checking-producer gap; got zero and `diagnostics: 0 errors`. Slow-producer certification race. | [36843386006](https://github.com/cortexkit/aft/actions/runs/36843386006/attempts/1), [36901251069](https://github.com/cortexkit/aft/actions/runs/36901251069/attempts/1) → 2 |
| `inspect_engine_test::inspect_engine_roots_share_bounded_named_thread_pool` | Windows | 1 | 09-30 13:48 | `matches!(first.join().expect("first scan"), JobOutcome::Fresh { .. })`. Shared queue/admission. | [36724403748](https://github.com/cortexkit/aft/actions/runs/36724403748/attempts/1) → 2 |
| `lsp_diagnostics_test::watcher_overflow_sends_no_per_file_flood` | Windows | 1 | 09-30 11:49 | `2 notifications`, expected 1. Watcher forwarding/barrier race. | [36710915667](https://github.com/cortexkit/aft/actions/runs/36710915667/attempts/1) → 2 |
| `per_checkout_9::callgraph_ops_report_a_keyless_view_generation_as_disabled` | macOS | 1 | 10-02 22:15 | Expected `callgraph_unavailable`; got `callgraph_building`. View publication/maintenance race. | [37071482497](https://github.com/cortexkit/aft/actions/runs/37071482497/attempts/1) → 2 |
| `per_checkout_9::standalone_view_publication_never_blocks_requests` | Windows | 1 | 10-05 13:53 | `a request waited 6002 ms for the view publication` (5 s assertion). Scheduling-sensitive latency assertion. | [37320286746](https://github.com/cortexkit/aft/actions/runs/37320286746/attempts/1) → 2 |
| `shared_db_contention_test::status_answers_within_a_second_while_another_process_holds_the_write_lock` | Windows | 2 | 10-03 21:30 | `status exceeded 1s while aft.db was write-locked: [1.1683217s]`. Sampling-window/latency assertion. | [36626746920](https://github.com/cortexkit/aft/actions/runs/36626746920/attempts/1), [37155371550](https://github.com/cortexkit/aft/actions/runs/37155371550/attempts/1) → 2 |
| `standalone_search_deferred_test::standalone_inspect_preserves_partial_results_when_rust_keeps_indexing` | Windows | 1 | 10-06 06:09 | Expected `E? W?` in partial inspect text; response had unknown diagnostics and no status-bar fragment. Partial-status rendering/readiness. | [37422176496](https://github.com/cortexkit/aft/actions/runs/37422176496/attempts/1) → 2 |
| `status_bar_text_test::standalone_status_bar_trails_text_on_change_and_not_on_unchanged_result` | Windows | 2 | 09-25 03:02 | `aft_inspect failed`, `dead_code ... callgraph_unavailable`. Background maintenance/readiness. | [36077367695](https://github.com/cortexkit/aft/actions/runs/36077367695/attempts/1), [36088716976](https://github.com/cortexkit/aft/actions/runs/36088716976/attempts/1) → 2 |
| `subc transport parity sweep > server-rendered text matches NDJSON for representative tool calls` | Linux, opencode-subc | 1 | 09-30 13:48 | `grep` text differed only by `1 indexed file(s) were not on disk ... index is out of date`. Fixture self-indexing/readiness. | [36724403748](https://github.com/cortexkit/aft/actions/runs/36724403748/attempts/1) → 2 |
| `subc_bridge_test::subc_bridge_goodbye_cancels_queued_read_before_same_root_rebind` | Windows | 1 | 10-01 07:39 | `subc mode exits cleanly: ConnectionLost`. Transport teardown. | [36831632794](https://github.com/cortexkit/aft/actions/runs/36831632794/attempts/1) → 2 |
| `subc_bridge_test::subc_bridge_health_check_returns_root_status_report` | Windows | 1 | 09-20 19:34 | `connection teardown must cancel queued root maintenance`. Queue teardown ordering. | [35532736043](https://github.com/cortexkit/aft/actions/runs/35532736043/attempts/1) → 2 |
| `subc_bridge_test::subc_bridge_l3_coalesces_already_bound_route_burst` | Linux | 1 | 09-22 02:19 | `subc mode exits cleanly: ConnectionLost`. Transport teardown. | [35679021255](https://github.com/cortexkit/aft/actions/runs/35679021255/attempts/1) → 2 |
| `subc_detach_test::subc_drain_exits_with_many_live_lsp_servers_in_one_deadline` | macOS | 1 | 09-30 23:00 | `exit took 2.129190417s` (2 s assertion). Shutdown latency/scheduling. | [36788805779](https://github.com/cortexkit/aft/actions/runs/36788805779/attempts/1) → 2 |
| `subc_detach_test::subc_drain_with_slow_writer_persists_terminal_line` | Linux | 1 | 10-03 21:30 | `shutdown did not save half the index wait by overlapping: combined=1521 ms, sequential=1702 ms`. Timing used as proxy for phase overlap. | [37155371550](https://github.com/cortexkit/aft/actions/runs/37155371550/attempts/1) → 2 |
| `trace_to_symbol_test::trace_to_symbol_no_path_reports_complete_no_path_found` | Linux | 1 | 10-05 11:17 | `timed out after 60s waiting for response line from aft stdout (child still running)`. Product/maintenance liveness. | [37301998630](https://github.com/cortexkit/aft/actions/runs/37301998630/attempts/1) → 2 |

Total: **27 names, 32 confirmed failing events**. Historical names are retained
above; a rename/removal in the current tree is not silently counted as a fix.

## Static inventory at 0a6eeb56

Searched `crates/aft/src` (including inline lib/bin tests), `crates/aft/tests`
and `packages/*` for `elapsed()`, `Instant::now() +`, `sleep`, `setTimeout`,
`toBeLessThan`, environment mutation, process globals and loopback ports.
This is a risk triage, not a statement that every sleep is wrong: a bounded
poll reads an actual condition, an injected producer delay exercises a
mechanism, and a negative receive timeout must remain short.

### High-risk timing assertions and sleep-then-assert fixtures

| Area/files | Risk / mechanism to inspect |
| --- | --- |
| `src/subc/push.rs` | Two `< 50 ms` assertions on saturated queues. Prove return while consumers have not drained; retain bounded channel waits only against hangs. |
| `src/inspect/manager.rs` (`tier2_deadline_tests`) | 250 ms sleeping producer and `< 250 ms` cancellation assertion. Gate the producer until permit release; assert cancellation/permit census. |
| `src/commands/inspect.rs` (`deferred_terminal_tests`) | Remaining wall-clock budget asserted after a receive. Assert the selected phase deadline/reserve before waiting, and the terminal outcome before releasing the producer. |
| `src/views/parent/tests.rs`, `tests/integration/per_checkout_9.rs` | 600 ms/2 s configure/discovery bounds; 3 s delayed discovery; 5 s publication latency. Need a controllable discovery/publication gate rather than a delay whose completion races the assertion. |
| `src/commands/health_digest.rs` | Passive digest `< 250 ms`. Existing LSP/cold-build counters prove no spawning, not that quiescence was never waited on. |
| `src/callgraph_store/mod.rs`, `src/readonly_artifacts.rs` | `< 250 ms`/1 s locked-schema read assertions; fixed 100 ms sleep before releasing writer. Prove return with the writer still held. |
| `src/effective_path.rs` | 4.5 s and 8 s probe assertions against 10 s sleeping shells; total budget should be checked via candidate invocation/kill markers. |
| `src/commands/bash_regex_match.rs` | `< 1 s` used as a complexity proxy. Requires algorithm/operation-count instrumentation to prove linearity without a performance assertion. |
| `src/subc/mod.rs`, `src/subc/wire.rs` | Deadline/teardown tests contain elapsed assertions; distinguish protocol deadline selection from scheduler timing. |
| `tests/integration/subc_detach_test.rs` | 2 s exit and budget+150 ms LSP assertions. Existing phase-event hook proves overlap; server census and orphan checks prove teardown. |
| `tests/integration/shared_db_contention_test.rs` | Historical 1 s sampling replaced by a held-lock fixture at base, but a response latency ceiling remains. |
| `tests/integration/standalone_search_deferred_test.rs`, `lsp_manager_test.rs`, `state_commands_test.rs` | Fixed slow producers and `< 2/5/15 s` assertions. Need producer-release barriers for ordering, not a sleep race. |
| `tests/integration/bash_reply_timing_contract_test.rs`, `bash_background_test.rs`, `bash_foreground_background_architecture_test.rs` | Completion/timeout wall-clock contracts and fixed sleeps. Existing process-sentinel fixtures can replace some bounds; polling bounds alone are not evidence of the intended timeout path. |
| `src/executor/tests.rs`, `src/subc/stall_watchdog.rs`, `src/context.rs`, `src/config_live_tests.rs` | Sleep-then-check progress/debounce/queue state. Check whether the test has an injected clock, started signal or drain barrier before changing it. |
| `packages/aft-bridge/src/__tests__/subc-transport.test.ts`, `subc-route-open-reload-window.test.ts` | Real-time route-open/call-budget bounds mixed with deterministic fake-clock assertions. Fake-clock comparisons are not flaky wall-clock assertions. |
| `packages/aft-bridge/src/__tests__/bash-host-fallback.test.ts`, `bash-hints.test.ts` | Abort/performance wall-clock assertions. Assert child cancellation or unresolved-stage ordering instead. |
| `packages/opencode-plugin/src/__tests__/subagent-detect.test.ts` | Lookup timeout ±50/500 ms assertions on loaded event loops. Assert fallback while host lookup remains unresolved and timeout result caching. |
| `packages/opencode-plugin/test/cancellation/effect-cancellation.test.ts` | `< 250 ms` interruption assertion. Assert finalizers and unresolved work ordering. |
| `packages/aft-bridge/src/__tests__/config-watch.test.ts`, `npm-resolver.test.ts`, `revivable-transport.test.ts`; `packages/opencode-plugin/src/__tests__/tui-notification-socket.test.ts`, `v2-result-metadata.test.ts`, `auto-update-checker/cache.test.ts` | Fixed sleeps before notification/revival/process/cache assertions; require actual condition/barrier. |

Paths in this table beginning `src/` or `tests/` are under `crates/aft/`.

### Shared process state

- Rust PATH tests in `src/tool_path.rs` mutate PATH. The process-env lock only
  serializes participants; unrelated LSP/git/child tests can still read it.
  Prefer a subprocess environment or an injected lookup (production API change).
- Rust env mutations also occur in `src/main.rs`, `src/context.rs`,
  `src/cli/warmup.rs`, `src/gh_shim.rs`, `src/bash_background/mod.rs`,
  `src/test_storage.rs`, `src/inspect/manager.rs` and integration fixtures
  (`branch_switch_test`, `per_checkout_semantic_runtime`, `subc_bridge_test`,
  `subc_storm_test`). RA/test hook settings can leak to concurrent readers even
  where a writer holds the environment lock.
- Global executor/cold-build admission, inspect engine queue, parent registry,
  bash registry and refresh/debounce settings must be checked for root-keyed
  ownership or isolated test constructors. A global mutex is not per-test state.
- TypeScript env/global mocks appear in bridge `paths`, `migration`,
  `onnx-concurrent-install`, Pi config/init fixtures, OpenCode `tui-preferences`,
  and CLI doctor/host-shell tests. `acquireEnv` and serial suites protect
  cooperating mutators, not arbitrary concurrent readers. New test-only
  subprocess fixtures should receive their own HOME/XDG namespace.
- Loopback `127.0.0.1:0` listeners in semantic, watcher, subc and fake-server
  fixtures reserve their sockets while serving; these are not fixed-port
  collisions. URL literals in config goldens (4096/11434) are not listeners.
  A port returned after dropping its reservation remains a race.
- Storage helpers already require explicit isolated `storage_dir` for Rust
  contexts. Test commands must still clear ambient `AFT_STORAGE_DIR`, and child
  processes need isolated HOME/XDG directories, not the operator's store.

## Verification and follow-up

This first commit preserves evidence and the static inventory before test
changes. Per-area fixes, mutation outputs, stress counts and explicit reasons
for tests requiring production changes will be recorded below. No ranking
files or production behavior are changed by this investigation.

The first Mac baseline compile queued behind other worktrees for 30 minutes
and was terminated before tests ran. Subsequent native checks use the remote
Linux build service per the task reviewer's direction; Windows compilation
remains the named local cross-check.

### Batch: Rust inspect deadline mechanisms

- The ignoring producer now waits for a release signal, not a 250 ms sleep.
  The test checks permit release while that producer cannot have completed;
  its root and limiter are per invocation. Positive waits have a 30 s hang
  ceiling.
- The cooperative test now asserts observed cancellation, the deadline-owned
  timeout outcome and an empty permit census, not `< 250 ms` elapsed time.
  Its name changed from `returns_within_grace` to `observes_cancellation` to
  disclose that this is a mechanism contract, not a latency SLA.
- The shared-request test checks the reserved terminal budget structurally
  and the named phase-timeout outcome while its producer is still gated. It
  no longer asserts remaining wall-clock budget after scheduler wakeup.
- Remote Linux gate: `cargo test -p agent-file-tools --lib --
  tier2_deadline_tests shared_request_deadline_returns_a_named_tier2_terminal_before_slow_work
  --test-threads 4`: **3 passed, 0 failed**.
- Mutation: withholding the permit from the deadline controller failed only
  `tier2_pass_deadline_releases_limiter_while_ignoring_stub_still_runs` with
  `deadline did not release limiter slot`; both other tests passed.
- Mutation: completing without cancellation failed only
  `tier2_pass_deadline_cooperative_stub_observes_cancellation` with
  `deadline must own the cancellation request`; both other tests passed.
- Mutation: sending Fresh before releasing the slow producer failed only
  `shared_request_deadline_returns_a_named_tier2_terminal_before_slow_work`
  with the `inspect_phase_timeout` outcome assertion; both other tests passed.
- Each control used staged live files and had a non-empty `git diff --stat`
  during the mutation and an empty one after `git checkout --` plus `touch`.
- Stress: temporary test-side helpers ran **each of the three changed tests
  20 times, with four concurrent contenders** (four lanes × five iterations).
  Output: `both tier2 deadline tests passed 20/20 with four concurrent
  contenders`; `shared-request deadline test passed 20/20 with four concurrent
  contenders`. **2 helper tests passed, 0 failed**. Helpers were removed and
  the final-source 3-test gate passed again. No retries or synthetic CPU load.
- `cargo fmt --all -- --check`: exit 0, rustfmt 1.10.0-stable. Compiler baseline
  is rustc 1.99.0 / cargo 1.99.0; the remote commands reported `ran remotely on
  ck-motor`.

### Batch: saturated subc push queues

Both `< 50 ms` assertions now observe a producer's return signal **before
any consumer drains the full queue**, with a 30 s hang ceiling. The original
reliable-bypass, overflow coalescing, dropped-frame and queue-content assertions
remain. All channels/roots are owned by the individual test.

- Remote final-source gate (both named tests, `--lib`, `--test-threads 4`):
  **2 passed, 0 failed**.
- Replacing overflow coalescing with a blocking send failed only
  `progress_sender_keeps_reliable_off_saturated_lossy_funnel_without_blocking`:
  `sender returned with saturated lossy queue still undrained: Timeout`.
  The fan-out test passed.
- Replacing writer try-enqueue with a blocking send failed only
  `fan_out_lossy_push_frame_drops_when_writer_is_full_without_blocking`:
  `fan-out returned with full writer queue still undrained: Timeout`.
  The progress-sender test passed.
- Staged-state mutation diffs were respectively `1 file, +2/-1` and
  `1 file, +5/-7`; each restore had an empty working diff.
- Temporary stress helper: **20/20 per test**, four lanes × five iterations,
  no retries. Output: `each saturated push test passed 20/20 with four
  concurrent contenders`. **1 helper passed, 0 failed**; it was removed and
  the final 2-test gate passed. Rust formatting passed again.

### Batch: shutdown and database lock fixtures

- The held-write-lock status test no longer samples request latencies. Five
  successful responses must arrive while its transaction is still open;
  `is_autocommit()` checks that ordering after each response. The response
  wait is a 30 s hang ceiling. Child storage uses an isolated cache root, not
  `AFT_STORAGE_DIR`; the existing spawn helper supplies disposable HOME/XDG.
- Shutdown now asserts one admitted **1500 ms** LSP phase, one server summary,
  the durable terminal marker, existing phase-event overlap and no surviving
  child PIDs. The outer 2 s and tight budget+150 ms comparisons are removed.
  The corrected check below bounds AFT's own LSP phase at two shared budgets;
  the outer process-exit elapsed observation remains diagnostic. PID disappearance is condition-polled
  with a 30 s ceiling.
- Initial remote fixture failure: `InsecureParentDirectory { ... mode: 509 }`
  (0775). Connection directories now explicitly use 0700, independent of the
  runner umask; the real connection security checks are not weakened.
- Native fixture build: `cargo build -p agent-file-tools --bin aft --features
  test-timing-hooks`, Finished. Final focused integration gate: **3 passed**.
  Affected `subc_detach_test` and `shared_db_contention_test` suites: **14 passed,
  0 failed**, `--test-threads 4`. The existing synthetic `CpuHog` test was
  excluded at invocation (`--skip
  subc_drain_exit_stays_bounded_when_lsp_servers_ignore_sigterm_under_load`)
  because the load rule forbids synthetic busy-loop stress. No source ignore
  was added.
- Mutations, all staged first and restored with an empty working diff:
  - COMMIT before status requests: only the held-lock status test failed,
    `the writer must remain locked until every status response has arrived`;
    both shutdown tests passed. Delta: shared DB fixture, +2.
  - LSP admission budget 1500 → 1400 in the **actual rebuilt binary**: only
    `subc_drain_exits_with_many_live_lsp_servers_in_one_deadline` failed,
    `all servers must be admitted under one 1500 ms shutdown budget`; status
    passed. Delta: LSP manager, +2/-1. A first 3000 ms control was rejected by
    the pre-existing compile-time exit-budget assertion before tests ran.
  - Joining index flush before LSP shutdown in the **actual rebuilt binary**:
    only `subc_drain_with_slow_writer_persists_terminal_line` failed,
    `index flush and LSP shutdown intervals did not intersect`; status and
    the other shutdown test passed. Delta: subc module, +3.
  - Connection fixture mode 0775: only the many-server shutdown test failed
    with `InsecureParentDirectory ... mode: 509`; status passed. Delta:
    connection fixture, +2/-1. A 0755 control passed: the connection-file
    contract forbids group write, not group read/traversal. This non-red
    boundary control is reported rather than misrepresented as a proof.
- Stress: a removed test-side helper ran **all three named tests 20/20**,
  four lanes × five iterations, with real concurrent module/fake-server work
  (not CPU hogs). Output: `both shutdown tests and locked-status test passed
  20/20 with four concurrent contenders`; **1 helper passed, 0 failed** in
  63.62 s. The restored real binary and final test sources were reverified.

### Batch: TypeScript lookup and issue-body fixtures

- V1/V2 timeout cases each own a dynamically imported cache/clock instance.
  The host cannot complete until the test releases it. Assert the primary
  fallback, an actually started but incomplete host lookup, and cached timeout
  behavior; the timer remains real. Remove ±50/500 ms elapsed assertions and
  use a 30 s per-case hang ceiling. The V1 test name explicitly changes from
  `within the timeout` to `before the host completes` to disclose the new
  ordering contract.
- The issue-body assembly fixture is 500 lines instead of 5000. It explicitly
  proves **96,952 uncapped bytes > 60,000 byte limit**, so reducing unrelated
  sanitization work does not remove cap coverage. Its original capped-size
  and retained-tool-failure assertions remain.
- Mutations: removing the host/deadline race failed exactly the V2 abandonment
  test in one run and the V1 fallback test in another, each at its 30 s hang
  ceiling. The three selected shared-lookup controls passed in each run.
  Bypassing the body cap failed only the assembly test (`Expected <= 60000;
  Received 96952`); aggregate-count control passed. Deltas: lookup source
  +2/-1; cap fixture +2/-1. Both restored working diffs were empty.
- Lookup stress: removed helper, **20/20 each**, four concurrent lanes, output
  `V1 and V2 timeout fixtures each passed 20/20 with four concurrent contenders`;
  **1 helper passed**, 200 expectations. Final lookup suite: **19 passed**,
  45 expectations.
- CLI stress: `bun run --cwd packages/aft-cli test:unit --test-name-pattern
  'doctor --issue body integration|capBodyToGithubLimit|aggregateBridgeToolFailures'
  --rerun-each 20 --concurrent --max-concurrency 4`: **280 passed, 0 failed**,
  740 expectations (14 tests × 20; repeats are not retries). Final-source
  suite: **14 passed**, 37 expectations.
- `bun run --cwd packages/opencode-plugin typecheck` and
  `bun run --cwd packages/aft-cli typecheck`: passed, TypeScript 5.9.3.
  `bun run lint`: **Checked 695 files**, passed, Biome 2.4.7. An initial import
  wrapping formatting error was corrected before the final gates. Bun 1.4.2.
  All local TS runs clear `AFT_STORAGE_DIR` and use disposable HOME/XDG dirs.
- Scoped `aft_inspect` was PARTIAL (no analyzed files/callgraph view in this
  worktree); it is not a clean diagnostic proof. Package tsc gates above are
  the authoritative verification.

### Delivery boundary and Windows gate

The reviewer approved delivery of these **11 verified test fixes** rather
than loosening assertions exposing unresolved product races. The remaining
confirmed flakes are dispositioned below for separate targeted work. No
ranking-fence file, package manifest/lockfile, generated schema, or production
behavior was changed. Native Rust checks ran remotely on ck-motor after the
Mac compile-slot stall; the remote service owns its own concurrency budget.

The requested local command (with `CARGO_BUILD_JOBS=4` and isolated HOME/XDG,
no `AFT_STORAGE_DIR`):

`CARGO_BUILD_RUSTC_WRAPPER= RUSTC_WRAPPER= RUSTFLAGS="-D warnings -A deprecated"
cargo check --target x86_64-pc-windows-gnu --tests -p agent-file-tools`

**Passed**, `Finished dev profile ... in 4m 23s` (rustc/cargo 1.99.0). This
compiles all Windows test targets; it does not prove Windows runtime behavior.
Native Windows verification is left to CI; the reviewer's Windows VM is down
for maintenance and was not used.

## Disposition of every confirmed CI flake

Locations below are current at this delivery, except the explicitly marked
historical Pi test. Run IDs refer to the linked failing attempts in the
confirmed table, not unrelated failures. **Suspected** causes are not claimed
as proved product defects: a same-SHA red/green pair proves nondeterminism,
while the failure assertion and current fixture determine the next diagnostic
step. The reviewer approved these remaining items as separate follow-ups.

| Confirmed test | File:line | Failed runs | Disposition, evidence and smallest proposed follow-up |
| --- | --- | --- | --- |
| `Pi enabled config toggle > disabled config registers nothing without resolving a binary or creating a bridge pool` | `packages/pi-plugin/src/__tests__/enabled-disabled-init.test.ts:73` (historical) | 35741038342 | **Retired before this task.** The old test slept 600 ms before reading a log and received an empty string. The current file tests config-error registration with logger spies (`bootInErrorState`, line 75), not a delayed disk flush. Preserve those mechanism assertions; no additional change proposed. Do not restore a sleep-based log check. |
| `bash tool adapter > transport-dead fallback recovers on the next successful module call` (Pi) | `packages/pi-plugin/src/__tests__/bash.test.ts:987` | 35548243286 | **Deferred; root cause not established.** The 20 s timeout happened during a real host-shell fallback, before the successful module recovery assertion. Suspect child startup/completion or cancellation cleanup, not incorrect recovery text. Smallest next step: add per-call spawned/stdio-EOF/reaped phases to the fallback fixture; split mock adapter recovery from the real-host lifecycle integration check. Do not only widen its timeout. |
| `bash_status_legacy_persisted_task_is_quarantined_on_replay` | `crates/aft/tests/integration/bash_background_persistence_test.rs:975` | 35407268324 | **Deferred product/sweep ordering.** The legacy file remained although direct quarantine succeeded. Current test waits for the recorded GC thread; the API documents that recording occurs at sweep completion. Therefore simple scheduler waiting is already present. Add root-scoped sweep generation/entry-discovery and quarantine-complete hooks, then ensure inline replay/coalesced GC cannot mark a sweep complete before its discovered legacy entries are renamed. Preserve the file-removal assertion. |
| `pty_windows_wrapper_script_runs_utf8_command` | `crates/aft/tests/integration/bash_pty_test.rs:1028` | 37225148728 | **Deferred Windows terminal-output race.** Completed status was observed with output containing only console negotiation/title escapes, no UTF-8 payload. Suspect terminal status preceding PTY reader EOF/final output publication. Smallest proposed production fix: order terminal-task publication after PTY drain completion, with a fixture hook holding the final output until that phase. Native Windows CI is required; compile-only success does not settle this. |
| `pty_write_python_repl_round_trip` | `crates/aft/tests/integration/bash_pty_test.rs:579` | 35420238745 | **Readiness already strengthened at base; native follow-up.** Current test version-probes Python and waits for `>>>` before writing. The failing log ended at that prompt without `pty-repl-ok`. Preserve the round-trip assertion. If it recurs, add write-accepted/PTY-read sequence hooks and verify console input delivery after the prompt; do not replace the payload assertion with terminal status. |
| `bash_completed_frame_compressed_tokens_reflect_compression` | `crates/aft/tests/integration/bash_token_count_test.rs:163` | 36097850252, 36706127153, 36780704669 | **Deferred completion/compression snapshot race.** `compressed_tokens == original_tokens == 613`, and output preview was a truncated raw git-status tail. There is no elapsed assertion to loosen. Smallest proposed fix: establish pipe EOF and final compressor-snapshot publication before building the terminal frame; expose a completion-snapshot phase hook and gate it in the fixture. Keep strictly smaller compressed-token count for this compressible payload. |
| `callgraph_rust_use_imported_short_callers_resolve` | `crates/aft/tests/integration/callgraph_test.rs:1796` | 37150941148 | **Deferred product liveness.** A tiny manifest/import fixture waited 60 s for any stdout response, child still alive; no fixed sleep or narrow test SLA. Add queued/admitted/builder-done/response-written job-ID tracing at cold-build admission, then fix the pending response or maintenance ownership that fails to release. Keep the imported-caller resolution assertion and fail on a missing response. |
| `doctor --issue body integration > tool failures section is included in assembled body and survives cap helper` | `packages/aft-cli/src/lib/bridge-tool-failures.test.ts:138` | 37098997194 | **Fixed here.** The old 5000-line sanitization workload timed out at Bun's 5 s default. The new 500-line fixture still exceeds the cap by 36,952 bytes and fails when the cap is bypassed. 20 repetitions passed under concurrent suite execution. |
| `e2e semantic search tool > aft_search degrades to a lexical fallback when semantic is disabled` | `packages/opencode-plugin/src/__tests__/e2e/semantic-search.test.ts:173` | 36508835945 | **Deferred lexical readiness/coverage.** Exact failure: expected `src/lib.rs:1 [lexical match]`, received `Found 0 results.\nSearch status: partial/incomplete.`. This is not the older obsolete-reason-text assertion recorded in the e2e verification note. Smallest next step: root/plane readiness ticket hook before the fixture's query; investigate whether the product serves an empty cold lexical spine without the needed coverage gap. Preserve actual lexical match assertions. Any ranking-source fix needs the ranking fence/evidence gate separately. |
| `valid_cache_skips_sleeping_shell_and_returns_ping_quickly` | `crates/aft/tests/integration/effective_path_cache_test.rs:193` | 36012527382 | **Mechanism test already at base; defer empty PATH cause.** It now checks that the probe marker was never written and that the served PATH contains the cache entry, not an elapsed bound. Failure was `served PATH ""`. Suspect cached-path publication vs first bash request or child-shell environment handling. Add a cache-loaded/path-applied phase and child argv/env capture; fix the ordering or fixture shell mode indicated by that evidence. Do not accept empty PATH. |
| `scoped_rust_inspect_drops_a_fixed_compiler_error_with_real_rust_analyzer` | `crates/aft/tests/integration/inspect_command_test.rs:5967` | 36796192789 | **Deferred real-server generation/readiness.** Even the initial control missed the compiler-only moved-value error. Suspect an idle/quiescent server report being accepted before the relevant cargo-check completion or diagnostic batch. Add check-start/check-end generation plus diagnostics-drained barrier; require the initial fixture's completed generation before editing, and keep the after-edit stale-error prohibition. Run with `AFT_TEST_REQUIRE_RUST_ANALYZER=1`. |
| `scoped_rust_inspect_reports_a_removed_field_after_an_outside_edit_with_real_rust_analyzer` | `crates/aft/tests/integration/inspect_command_test.rs:5931` | 37331104560 | **Deferred watcher/diagnostic generation race.** The fixture edits unopened `src/s.rs`, drains a supplied watcher event, then scopes to the use site; RA's removed-field error was missing. Smallest next step: correlate forwarded watched-file notification with RA analysis/check generation; fix forwarding or freshness certification if a clean result is issued before that generation. Preserve the outside-edit path and require real RA; polling repeated inspect results would be a retry, not a fix. |
| `unscoped_inspect_names_an_unfinished_cargo_check` | `crates/aft/tests/integration/inspect_command_test.rs:6261` | 36843386006, 36901251069 | **Deferred certification race.** The fake check already uses `FLYCHECK=never`, with begin deliberately delayed 500 ms after quiescent status. The response had zero Rust checking-producer gaps (expected one), and text certified `diagnostics: 0 errors` while Tier-2 alone kept overall `complete: false`. That fixture intentionally exposes the idle-before-check-begin window. Smallest production fix: make unscoped freshness depend on the relevant check generation/end, not temporary idle status; add a begin-held release signal to fake LSP for deterministic ordering. Keep the unknown-diagnostic assertion. |
| `inspect_engine_roots_share_bounded_named_thread_pool` | `crates/aft/tests/integration/inspect_engine_test.rs:311` | 36724403748 | **Deferred shared-pool deadline fixture.** Managers share the actual pool; worker bodies sleep 150 ms under 2 s work budgets. First result was not Fresh. Possible queued/in-flight deadline cancellation under global contention; product defect not established. Smallest fixture-only follow-up: workers announce start and wait for a per-test release signal, then assert owned completion/census and pool identity with a generous hang budget. Capture non-Fresh outcome before deciding whether root cancellation is leaking across jobs. |
| `watcher_overflow_sends_no_per_file_flood` | `crates/aft/tests/integration/lsp_diagnostics_test.rs:3269` | 36710915667 | **Deferred duplicate forwarding.** Two watched-file notifications arrived where one oversized batch should forward only Cargo.toml. Current helper waits for registration and uses ordered didChange acknowledgement, so it is not merely sleep-then-count. Add a batch-ID queue/drain hook around direct vs contended forwarding; guarantee exactly one delivery and apply the cap before either path. Keep the one-notification and configuration-only assertions. |
| `callgraph_ops_report_a_keyless_view_generation_as_disabled` | `crates/aft/tests/integration/per_checkout_9.rs:1550` | 37071482497 | **Deferred bootstrap/publication ordering.** Current fixture sends 40 ping requests with 20 ms sleeps while an 8 s republish delay runs. The expected disabled current-view state instead yielded `callgraph_building`, and can race installation/publication. Smallest hook: root-scoped current-view-installed signal plus publication release gate; query the keyless generation only after installation and before release, then prove graph availability after commit. Do not tolerate building/unavailable indiscriminately before the keyed generation exists. |
| `standalone_view_publication_never_blocks_requests` | `crates/aft/tests/integration/per_checkout_9.rs:1302` | 37320286746 | **Deferred genuine publication gate needed.** A grep took 6002 ms against a 5 s assertion during a timed 20 s publication. A larger sleep or bound still races scheduler delays. Smallest test hook: signal publication started, hold it on a root-local release gate, prove responses while the gate is closed, release and poll the published generation. No inline/blocking publication should satisfy the ordering. |
| `status_answers_within_a_second_while_another_process_holds_the_write_lock` (historical name) | `crates/aft/tests/integration/shared_db_contention_test.rs:235` (`status_answers_while_another_process_holds_the_write_lock`) | 36626746920, 37155371550 | **Corrected here.** Keep the transaction open and require five status replies plus real database reads while an observed maintenance BUSY retry is held behind a release gate. A rebuilt production mutex-held retry control fails this case; the earlier COMMIT-only control was insufficient. No request latency SLA is used. |
| `standalone_inspect_preserves_partial_results_when_rust_keeps_indexing` | `crates/aft/tests/integration/standalone_search_deferred_test.rs:1057` | 37422176496 | **Deferred partial-status rendering/certification.** The fake advertises ongoing indexing; the structured response had unknown diagnostics, but the expected `E? W?`/partial text assertion failed. Retain unknown counts and scoped producer exclusion. Smallest follow-up: begin/ongoing-status acknowledgement hook before request plus correlation of status-bar tickets with the same inspect generation; remove its redundant 5 s elapsed assertion only once the producer is gated. |
| `standalone_status_bar_trails_text_on_change_and_not_on_unchanged_result` | `crates/aft/tests/integration/status_bar_text_test.rs:53` | 36077367695, 36088716976 | **Deferred Tier-2/callgraph generation readiness.** Second inspect failed with `dead_code ... callgraph_unavailable`, not an unchanged-bar comparison. Current test already allows a genuinely changed stale bar rather than requiring silence. Smallest next step: expose the graph/Tier-2 generation installed after the fixture edit and distinguish missing admission from a real failed build; fix dead-code projection readiness or gate fixture setup to that publication. Keep the T2 count and no-identical-repeat assertions. |
| `subc transport parity sweep > server-rendered text matches NDJSON for representative tool calls` | `packages/opencode-plugin/src/__tests__/e2e/subc-parity.e2e.test.ts:261` | 36724403748 | **Relevant fixture fixes already at base.** The grep difference was a stale indexed file missing from disk on one side. Current fixture excludes its own cache/user-state files and the convergence predicate rejects the exact stale-disk warning before comparison. Preserve byte-for-byte normalized parity. If it recurs, add an index-generation/watcher-drained barrier before parity, not removal of coverage warnings from normalized text. |
| `subc_bridge_goodbye_cancels_queued_read_before_same_root_rebind` | `crates/aft/tests/integration/subc_bridge_test.rs:2327` | 36831632794 | **Deferred transport teardown classification.** Final result was ConnectionLost rather than the required clean exit. Suspect fake-daemon close before Goodbye is consumed, or EOF winning the loop's classification after a valid Goodbye. Add a Goodbye-processed acknowledgement barrier in the fake transport and distinguish accepted Goodbye from bare EOF; fix whichever ordering the trace shows. Preserve queued-read cancellation and same-root rebind assertions. |
| `subc_bridge_health_check_returns_root_status_report` | `crates/aft/tests/integration/subc_bridge_test.rs:2997` | 35532736043 | **Cooperative-idle wait already at base; follow up if recurring.** Current test waits on actual actor idleness after Goodbye and asserts the context was quiesced. The failure named queued maintenance still alive. Add root/job cancellation-observed and final-idle hooks to separate queued-job leak from a running worker's final step; fix actor-owned maintenance cancellation if jobs survive that barrier. No immediate idle assertion should replace the poll. |
| `subc_bridge_l3_coalesces_already_bound_route_burst` | `crates/aft/tests/integration/subc_bridge_test.rs:2400` | 35679021255 | **Deferred teardown classification, not coalescing tolerance.** Same ConnectionLost final-state failure as the Goodbye/rebind case. Share the Goodbye-consumed/EOF classification diagnostic hook with that fixture, then fix clean-close ordering. Keep burst/coalescing assertions independent of teardown evidence. |
| `subc_drain_exits_with_many_live_lsp_servers_in_one_deadline` | `crates/aft/tests/integration/subc_detach_test.rs:537` | 36788805779 | **Corrected here.** Retain admission, single summary, durable exit and no-orphan conditions without the outer 2 s scheduler race. Require at least three servers and AFT's logged LSP phase elapsed below two shared budgets. A rebuilt sequential per-server shutdown retaining one summary fails the elapsed assertion. |
| `subc_drain_with_slow_writer_persists_terminal_line` | `crates/aft/tests/integration/subc_detach_test.rs:572` | 37155371550 | **Fixed here / existing overlap hook retained.** Ordered start/end phase events prove overlap, not a savings ratio. Serializing the real child phases fails only this overlap fixture; durable terminal marker still required after slow log flushing. |
| `trace_to_symbol_no_path_reports_complete_no_path_found` | `crates/aft/tests/integration/trace_to_symbol_test.rs:126` | 37301998630 | **Deferred product liveness.** A tiny unrelated-functions fixture waited 60 s for stdout, child still running. Share cold-build admission/response ownership tracing with the imported-callers case. Fix stalled publication or pending-response drain identified by those phases; retain complete no-path proof rather than interpreting timeout as no path. |

### Other static risks left for isolated follow-ups

The risk inventory is not claimed eliminated. Remaining narrow timing tests
for parent discovery/publication, passive health digest, locked-schema readers,
regex complexity, shell-probe total budget, executor/watchdog/debounce, bridge
route-open/abort and notification delivery need separately reviewed counters,
release gates or fake clocks. Existing environment locks do not isolate
nonparticipating readers: PATH/HOME/env-mutating tests need subprocess fixtures
or explicit resolver parameters. The remaining shared DB maintenance-read
latency case needs a held-maintenance-step hook to prove reads return before
that step is released; merely widening READ_BUDGET would hide the connection
mutex regression it is designed to catch. These broader fixture/test-hook
refactors are outside the reviewer-approved delivery boundary, not silently
claimed fixed. No retries, source ignores or production changes were added.

### Evidence collection gaps (reproducible follow-up)

The first failed-attempt download had transient failures for attempts:
`37506337591/1`, `37496360329/1`, `37422176496/2`, `36800107650/1`,
`36752821554/1`, `36468660052/1`, `36197002440/1`, `35979023573/1`,
`35971189540/1`, `35908155729/1`, `35827207512/1`, `35510928236/1`.
Focused job-log collection later recovered the relevant passing job for
`37422176496/2`; the other missing first-attempt logs were not inferred from
workflow conclusions. Fetch those exact attempts to extend this confirmed
lower-bound census. Run creation times, not failing-test log timestamps, are
used consistently in the Last seen column. The proposed race fixes above are
hypotheses supported by assertion text and source inspection, not verified
root-cause claims or permission to relax the tests.

## Review correction: maintenance/read regression proof

The original held-transaction/early-COMMIT control established fixture
ownership, but **did not establish that reads avoid maintenance's connection
mutex**: a request could wait out the 2 s busy budget and still return under
the 30 s hang ceiling. That earlier proof is superseded by the ordering check
below; its red fixture control must not be cited as defending this regression.

- `maintenance_write` has a debug/timing-build checkpoint after a real SQLite
  BUSY result and **after dropping the connection guard**. A spawning test
  supplies its own disposable `AFT_TEST_MAINTENANCE_RETRY_GATE` directory.
  The child logs `maintenance write busy retry paused`, publishes a `paused`
  marker, and cannot resume its busy retry until the test writes `release`.
  Release builds without timing hooks do not expose the checkpoint. Its
  abandoned-fixture ceiling is 120 s, longer than the 30 s request hang bound;
  it removes `paused` on exit so an old marker cannot certify an active retry.
- The status case waits for that actual busy-retry checkpoint, then requires
  **five status responses and five `db_get_state` reads before retry release**.
  The database probe is intentional: status can serve cached aggregates when
  a connection is held, which otherwise hides the maintenance/read regression.
  Every pair checks that the retry is still paused and the foreign write
  transaction is still open. Release is guarded on unwind and normal cleanup.
- Production mutation: replace `maintenance_write` with the same SQLite BUSY
  retry loop but acquire the connection mutex once and retain it across all
  retries/checkpoint waits. The **actual child binary was rebuilt**. Only
  `shared_db_contention_test::status_answers_while_another_process_holds_the_write_lock`
  failed: `timed out after 30s waiting for response line from aft stdout (child
  still running)`. The many-server LSP shutdown control passed (1 pass, 1 fail).
  This is not a COMMIT/fixture mutation. Applied delta: `crates/aft/src/db/mod.rs`,
  +12/-52; staged live state before mutation, empty working diff after restore
  and `touch`; restored actual child rebuilt before green verification.
- Remote stress helper (removed after execution): four concurrent lanes × five
  invocations, **20/20**. Output: `status and database reads during paused
  maintenance passed 20/20 with four concurrent contenders`; 1 helper passed
  in 7.08 s. No retries or CPU hogs.
- Final-source remote gates, all `--test-threads 4`: maintenance-write lib
  contracts **3 passed**; binary `tick_flushes_ready_pending_before_runtime_maintenance`
  **1 passed**; `shared_db_contention_test` integration suite **4 passed**.
  Native commands used the plain-cargo ck-motor route and the timing-hook
  feature. `cargo fmt --all -- --check` passed (rustfmt 1.10.0-stable).

## Review correction: shared vs sequential LSP shutdown

A single phase admission line and server summary did **not** distinguish a
shared deadline from N sequential per-server deadlines. The earlier admission
budget mutation is not a proof of concurrency, and dropping both internal
and outer elapsed checks removed that regression protection. The outer
process-exit comparison stays removed; AFT's own LSP measurement is restored
with a deliberately broad bound.

- The primary deadline case uses **four independent servers** (N ≥ 3); the
  other slow-start, slow-writer and active-ORT fixtures keep their 34 roots.
  Both fixture construction and the actual shutdown summary require N ≥ 3,
  preventing a one-server run from vacuously proving a shared budget.
- Parse AFT's `lsp shutdown_all: ... servers=N ... elapsed_ms=M` and require
  **M < 2 × LSP_SHUTDOWN_ALL_BUDGET** (currently 3000 ms). This is not the test
  thread's elapsed time or the outer exit observation. Admission, durable
  terminal marker, summary count and orphan checks remain.
- Production mutation: change `shutdown_all_clients` to drain each server
  sequentially under its own 1500 ms budget, retaining **one truthful summary**
  and the same `budget_ms=1500` admission line. The real child was rebuilt.
  Only `subc_detach_test::subc_drain_exits_with_many_live_lsp_servers_in_one_deadline`
  failed, specifically: `LSP shutdown took 4801 ms for 4 servers; expected less
  than two shared budgets (3000 ms)`. The paused-maintenance status/read test
  passed (1 pass, 1 fail). This proves the internal elapsed assertion, not the
  old outer 2 s SLA, rejects the actual sequential path.
- Applied delta: `crates/aft/src/lsp/manager.rs`, +17/-66; staged live source
  before mutation, non-empty working diff during the control, empty after
  `git checkout --` and `touch`. The restored child binary was rebuilt.
- Remote stress helper (removed): four concurrent lanes × five invocations,
  **20/20**, output `logged shared LSP deadline passed 20/20 with four concurrent
  contenders`; 1 helper passed in 9.44 s. No retries or synthetic load.
- Final affected integration suites: **14 passed, 0 failed**, `--test-threads 4`,
  excluding only the existing forbidden CPU-hog case at invocation, not by a
  source ignore. `cargo fmt --all -- --check` passed.
- Final local GNU Windows check with four build jobs, isolated HOME/XDG and no
  `AFT_STORAGE_DIR`: `CARGO_BUILD_RUSTC_WRAPPER= RUSTC_WRAPPER= RUSTFLAGS="-D warnings -A deprecated"
  cargo check --target x86_64-pc-windows-gnu --tests -p agent-file-tools` passed,
  `Finished dev profile ... in 2m 30s` (rustc/cargo 1.99.0). No Windows VM used.
- Scoped Rust `aft_inspect` remained PARTIAL because rust-analyzer was still
  indexing; it is not counted as a clean diagnostic result. Native lib/bin/
  integration gates and the GNU test-target compilation are authoritative.

This correction adds only the debug/timing checkpoint and the two strengthened
regression fixtures. It does not modify release maintenance or LSP shutdown
behavior, ranking files, package manifests or generated schemas. Original
CI-history limitations and other deferred follow-ups remain explicit above.

Final correction gates after stress-helper removal and comment cleanup:
`cargo test --features test-hooks -p agent-file-tools --features test-timing-hooks --test integration --
status_answers_while_another_process_holds_the_write_lock
subc_drain_exits_with_many_live_lsp_servers_in_one_deadline --test-threads 4
--nocapture`: **2 passed, 0 failed**. `cargo check -p agent-file-tools --lib
--release`: **Finished release profile**, passed; reported one unrelated
pre-existing unused `VecDeque` import in unchanged `src/cache_freshness.rs:4`.
No warning from the new checkpoint was emitted. The scoped inspector remained
incomplete, so no clean-inspection claim is made.
