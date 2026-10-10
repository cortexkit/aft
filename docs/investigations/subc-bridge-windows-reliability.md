# Native Windows subc bridge reliability

## Reproduction

The OVH Windows Server VM reproduced all seven reported failures at
`eb01b4ac2f3ae05c47bf0949c083b9a8d0198a56`: 93 passed, 7 failed, 2 existing
manual benchmarks ignored, in 280.95 seconds. The command was:

```sh
scripts/windows-gate.sh --base eb01b4ac2f3ae05c47bf0949c083b9a8d0198a56 --full --filter subc_bridge_test
```

The filter also selected an empty lib slice; that is not a passing lib check.
The Windows gate transfers committed Git objects and runs Cargo test slices on
the VM. Subsequent runs omitted `--full --filter`: it selected nonempty lib and
integration slices from the Rust files changed since the supplied base.
No maintenance lock was present at the initial `--status` check. All test code
sent to the VM was committed first.

## Causes and fixes

Class **a** means a product defect; class **b** means a fixture assumption that
fails under the VM's PowerShell shell.

| Test | Class | Evidence and change |
| --- | --- | --- |
| `subc_bridge_bash_promote_panic_triggers_fatal_teardown` | a | The VM logged the forced executor panic, but delivered a successful generic promotion response (`isError:false`) and exited with `ConnectionLost`. When its reply timer expired, the module stopped awaiting the executor's promotion result and sent a successful background-task handoff instead. The answer-ownership check incorrectly let that timer replace an answer already owned by the executor, losing the panic error and the fatal-teardown signal. The deadline now takes only an unclaimed answer; promotion claims when its mutating job starts, not while queued. An already-owned answer is awaited so its error and fatal teardown survive. The test holds its command until released and injects a 500 ms promotion delay, beyond the existing backstop, to reproduce the race on every platform. |
| `subc_bridge_bash_watch_regex_pattern_round_trips_validation` | b | The VM returned match offset 262, not 4: PowerShell's missing-`printf` diagnostic echoed the command containing `ready: 4242`. The old substring assertion accepted that diagnostic as command output. The producer now uses native line output and the test requires the complete expected line before scanning it; offset 4 is still asserted. |
| `subc_bridge_client_cancel_answers_held_bash_wait_and_keeps_command_running` | b | The start marker never appeared, so the driver never reached Cancel. Native marker creation now starts a command held by a release file. It is released only after the Cancel terminal and continued-running assertion, then its successful completion must arrive. |
| `subc_bridge_module_draining_answers_bash_wait_whose_poll_is_stuck` | b | The start marker never appeared, before the test occupied the reader lane or sent drain. Native marker creation and an explicit release keep the command running throughout the blocked-poll drain assertion; completion and absence of duplicate terminals remain checked. |
| `subc_bridge_module_draining_answers_every_held_request_and_keeps_bash_running` | b | The `wait:true` start marker never appeared. The later heavy-release condvar panic was secondary: the driver never reached release. Both shell commands now use native markers and release-file holds instead of six-second sleeps. All held terminals, census counts, continued execution and both completions remain asserted. |
| `subc_bridge_module_draining_closes_every_daemon_request_credit` | b | The bash start marker never appeared; the heavy-release panic followed the aborted driver. Both shell commands now use native markers and release-file holds instead of five-second sleeps. Every daemon credit must still close exactly once, including after command release. |
| `subc_bridge_worker_blocking_bash_detaches_at_the_worker_wait_limit` | b | The VM did hand off at 1.5 seconds and said the command was not killed, but its recent output was PowerShell's `printf` error, not `started`. Native line output plus a release-file hold replaces that producer. Both `wait` and `block_to_completion` retain the original timing, output, running-status and explicit-kill assertions. |

The six fixture fixes alone changed the VM result to 99 passed / 1 failed,
leaving only the promotion-panic defect. No test timeout was increased and no
test was newly skipped or disabled on Windows.

The successful background-task handoff sent when the reply timer expires also
failed to record the call in the repeat breaker. That made repeated commands on
this answer path invisible to the reminder mechanism. The handoff now uses the
ordinary bash result finalizer. Its distinct answer-ownership state prevents a
queued poll from recording a second result after the handoff; a separate claim
unit test checks that late polls, promotions and drain cannot answer again. A new unit test,
`bash_deadline_handoff_observes_repeat_while_task_lock_is_held`, holds a real
executor writer and task-state lock, requires a bounded handoff with the third
call's reminder, and checks exactly-once observation and no duplicate terminal.
Historical observation times seed the first two calls without thirty seconds
of real waiting. The existing transport repeat test remains unchanged.

## Consecutive VM evidence

At code commit `2b6efddd36ae700bc402cff2e0cc98e2e3ba248f`, three consecutive
runs of the following command executed the complete `subc_bridge_test::` slice:

```sh
scripts/windows-gate.sh --base eb01b4ac2f3ae05c47bf0949c083b9a8d0198a56
```

| Run | Integration result | Integration time | Gate wall time |
| --- | --- | --- | --- |
| 1 | 100 passed, 0 failed, 2 existing benchmarks ignored | 175.99 s | 843.93 s |
| 2 | 100 passed, 0 failed, 2 existing benchmarks ignored | 178.86 s | 646.57 s |
| 3 | 100 passed, 0 failed, 2 existing benchmarks ignored | 190.21 s | SSH interrupted at 259.61 s; completed guest result recovered |

Every one of the seven named tests passed **3/3**.
`subc_bridge_repeat_breaker_steers_bash_on_every_answer_path`, which checks repeat
reminders on rewritten, completed and promoted bash answers, also passed **3/3**.
`subc_bridge_without_discovered_status_line_surface_emits_no_status_requests`,
which checks that absent catalog advertisements suppress status traffic, passed
**3/3**. Both new unit tests (deadline repeat steering and the late-poll ownership
fence) passed **3/3** on the native VM.

Run 3's SSH output stream timed out after the integration suite started. The VM
remained reachable; its persistent run log contained the completed 100-pass
result and every named test. The log was retrieved using the Windows gate's SSH
helper, then the gate's normal cancellation-file protocol requested supervisor
cleanup. The supervisor released its lock; no lock was removed manually and no
maintenance lock was present. The local orchestration exit was 255 for this run,
not a test failure. Its full guest result, not a partial local stream, is the
third integration pass.

These are **integration-slice passes, not aggregate Windows gate passes**.
Each run also executed the broader `subc::` lib slice: 310 passed, 1 failed,
7 existing ignores. The unrelated
`exec_remote_v1_bash_runs_through_real_route_without_invalid_selector` fixture
still issues `printf v1-bash-proof`; its captured PowerShell diagnostic is
compared with `v1-bash-proof`. That unchanged fixture is outside this change's
scope. The first two local gate invocations exited 1 for it; the recovered third
guest log reports the same lib failure. Storage-isolation preflight passed
1/1 on every run. Native VM tools were Cargo 1.99.0 and rustc 1.99.0.

The reported timeout of the fake-daemon driver in
`subc_bridge_without_discovered_status_line_surface_emits_no_status_requests`
was not reproduced in these runs. Its driver still has a 30-second outer watchdog around a sequence that
includes 30-second consumer connection/authentication bounds. That potential
budget issue is not declared fixed: changing it without a reproduced event
would not establish the actual cause of the earlier CI failure.

## Other verification and break tests

- Linux, Cargo/rustc 1.99.0: `cargo test -p agent-file-tools --lib subc::`
  passed 337 tests (7 existing ignores); the integration slice passed 100 tests
  (2 existing ignores) after restoring all mutations.
- macOS, Cargo 1.99.0: `--lib subc::` passed 335 tests (8 existing ignores) and
  the integration slice passed 100 tests (2 existing ignores). HOME was a newly-created isolated
  directory, `AFT_STORAGE_DIR` was unset, and only Cargo/rustup toolchain homes
  used the existing tool installation. The native linker emitted its existing
  large-unwind-table warning.
- `cargo fmt --all -- --check` passed with rustfmt 1.10.0-stable.
- An earlier scoped rust-analyzer snapshot reported authoritative results for
  all three Rust files: 0 errors and 0 warnings. Final inspection remained partial
  while its cargo check was running and the checkout call graph was unavailable;
  the completed three-platform compilations and tests are the authoritative gates.
- A read-only AI comment review (Sidekick) covered all changed Rust files:
  3/3 files, 7 comment blocks, none flagged. A subsequent review of the added
  claim invariant and this report flagged prose clarity issues, which were
  clarified.

All mutations were applied after staging the live implementation and were
restored from that index, followed by an empty working-tree `git diff --stat`.
Each deliberate break carried an explicit temporary-mutation marker to identify
it as a change that proves a test can detect incorrect behavior. None was committed:

1. Replacing the deadline's exclusive claim with the old reentrant claim made
   only `subc_bridge_bash_promote_panic_triggers_fatal_teardown` fail in its
   one-test run: the caller received a successful background-task handoff
   instead of the panic error; exit was `ConnectionLost`.
2. Omitting the fallback finalizer made only
   `bash_deadline_handoff_observes_repeat_while_task_lock_is_held` fail in its
   one-test run: `deadline handoff must steer`.
3. Emitting an incorrect native line made the regex and worker-limit tests each
   fail in separate one-test runs (`wrong-abc ready: 4242` / `wrong-started`).
4. Writing a different start marker made each of the four drain/cancel tests fail
   separately at its own missing-marker assertion. The heavy-release errors
   occurred only as secondary panics within the two heavy-call scenarios.

5. Giving deadline handoff the ordinary wait-task ownership state made only
   `deadline_handoff_cannot_be_reclaimed_by_poll_or_promotion` fail in its
   one-test run: `a late poll must not finalize again`.

Full local evidence is retained outside build output in
`local-ignore/windows-subc/`; none of its logs or isolated homes is committed.
