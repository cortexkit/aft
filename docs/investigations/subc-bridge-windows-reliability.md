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
| `subc_bridge_bash_promote_panic_triggers_fatal_teardown` | a | The VM logged the forced executor panic, but delivered a successful generic promotion response (`isError:false`) and exited with `ConnectionLost`: the reply timer discarded the future observing the executor result. In the reviewed revision, executor work and status formatting finish before a result can reserve the caller's reply. The deadline can therefore return the task id while that work blocks, and a separate observer still reports a later fatal panic. The test proves executor entry, delays it for 3 seconds, requires a reply within 2 seconds, then requires both fatal Goodbyes (route and connection closure frames) and `ActorFatal` (the fatal exit error that allows supervision to restart the module). |
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
failed to record the call in the repeat breaker, which emits reminders for
repeated commands. That made repeated commands on this answer path invisible.
The deadline reply now records the delivered call using only in-memory repeat
state, without waiting for diagnostic or fleet-status locks. Ordinary executor
results finish status formatting first, then record a repeat only if selected
for delivery; they hash undecorated output and retain the status suffix.
The deadline's distinct answer-ownership state prevents a queued poll from
recording a second result after handoff; a separate claim
unit test checks that late polls, promotions and drain cannot answer again. A new unit test,
`bash_deadline_handoff_observes_repeat_while_task_lock_is_held`, holds a real
executor writer and task-state lock, requires a bounded handoff with the third
call's reminder, and checks exactly-once observation and no duplicate terminal.
Historical observation times seed the first two calls without thirty seconds
of real waiting. The existing transport repeat test remains unchanged.

## Initial verification before the reply-bound review

The three passes below belong to the initial delivery. That version still
waited without a bound if a promotion had already reserved its reply, which
was rejected in review. They are historical evidence, not verification of the
revised bound; the reviewed revision's results follow in the next section.

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

## Initial cross-platform verification and break tests

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

## Review revision: ownership only after work finishes

The review selected a later reply claim rather than assuming promotion is
bounded. `submit_bash_promote` no longer claims at executor entry. The executor
finishes promotion and status formatting, and the result receiver then reserves
the single reply immediately before in-memory repeat recording and delivery.
The same ready-result rule applies to foreground polls.

The work moved before reserving the single reply includes:

- Foreground/wait registration map mutexes in `release_wait_registration`.
- The task-map and task-state mutexes used by `BgTaskRegistry::promote`.
- Task metadata JSON writes under task state, followed by deferred database writes.
- For delegated workers limited by `bash.worker_wait_max_ms`, moving the task's
  kill deadline out while the worker waits, persisting it, and observing task status.
- Status-finalizer locks, including diagnostic/tier-2 caches and fleet-reader state.

The reply timer never awaits those operations. It sends a task-id reply and
leaves a separately spawned observer awaiting the executor result. If that
result is fatal, the observer sends a fatal-only event: it queues the fatal
Goodbyes and shutdown, but neither another tool response nor a second decrement
of root/route wait counts. A nonfatal late result is discarded without recording
another repeat or settling the same request again. Deadline replies omit status
formatting because its cache/reader mutexes could otherwise stall the reply.

The revised tests exercise promotion that has **started**, not just a promotion
waiting for the executor's writer lane:

- `subc_bridge_bash_started_promotion_hands_off_before_transport_deadline` waits
  for an executor-written start marker, while the job is delayed for 3 seconds.
  Its 200 ms foreground window plus existing reply backstop must produce a
  task-id reply within 2 seconds, comfortably inside the 25-second transport
  budget. A read queued behind this background-promotion job must receive its
  own correlation ID 117 after the job finishes; another bash reply would carry
  ID 116 and fail the test.
- `subc_bridge_bash_promote_panic_triggers_fatal_teardown` uses the same started
  delay, requires the initial bounded task-id reply, and then independently
  requires ordered route/connection Goodbyes and `SubcError::ActorFatal`. Normal
  background completion pushes may precede those Goodbyes.
- `late_promotion_fatal_signals_teardown_without_settling_hold_twice` exercises
  the actual late-event producer and handler. It requires fatal shutdown and
  both Goodbyes, no extra response, and unchanged sibling root/route hold counts.

The panic test's first-reply assertion intentionally changed from a panic error
to a deadline handoff so that the fatal-teardown property is tested independently
of the earlier reply. Its fatal exit and Goodbye assertions were retained.
The six class-b fixtures (native PowerShell marker/output commands and explicit
release files instead of fixed sleeps) were not changed from initial verification.

### Revised verification

The code commit containing ready-result reply ownership and the retained
late-panic observer, actually tested below, is
`9c7de7779f8f44f495bd622b6ba732569827b459`.

```sh
scripts/windows-gate.sh --base 9c9c89055a023075609bc3dbbe39679abc218d2e
```

The exact three native Windows runs are recorded below; no additional retries
were launched after the reviewer requested the actual outcomes.

| Run | Integration result | Integration time | Broader lib result | Gate wall time |
| --- | --- | --- | --- | --- |
| 1 | 101 passed, 0 failed, 2 existing ignores | 191.34 s | 311 passed, 1 failed, 7 ignores | 846.27 s |
| 2 | 101 passed, 0 failed, 2 existing ignores | 198.61 s | 311 passed, 1 failed, 7 ignores | 666.38 s |
| 3 | 100 passed, 1 unrelated failure, 2 existing ignores | 210.16 s | 310 passed, 2 unrelated failures, 7 ignores | 687.54 s |

All seven original failures, the started-promotion bound, delayed fatal teardown,
and late-fatal accounting tests passed **3/3**. This is **not** three consecutive
complete integration passes, and no aggregate Windows gate is declared passed.
Cargo/rustc were 1.99.0; storage-isolation preflight passed 1/1 on every run.
No maintenance lock was present at the initial status check.

Unrelated failures, individually:

- `subc::bash_selector_tests::exec_remote_v1_bash_runs_through_real_route_without_invalid_selector`
  — `crates/aft/src/subc/bash_selector_tests.rs:90`; assertion `left == right`
  failed, expected `"v1-bash-proof"`, received the PowerShell missing-`printf`
  diagnostic. This unchanged fixture failed in all three runs and in the prior
  delivery's runs. It was not separately run at the original base commit.
- `subc_bridge_test::subc_bridge_management_surface_is_passive_closed_and_first_party`
  — `crates/aft/tests/integration/subc_bridge_test.rs:11874`; exact assertion
  message: `census must carry process_io: {census:?}`. `/data/process_io` was absent
  in run 3; runs 1/2 passed. Reproduction at the base commit is not established.
- `subc::tests::standing_actor_lock_contention_does_not_block_route_binds_or_health`
  — `crates/aft/src/subc/mod.rs:11311`; exact assertion message:
  `released bind work must settle before executor teardown: Elapsed(())`.
  Its final 2-second cleanup wait failed only in run 3. Reproduction at the base
  commit is not established.

Linux: `cargo test -p agent-file-tools --lib subc::` passed 338 tests (7 existing
ignores). That combined run lost remote monitoring during integration; its
integration outcome is recorded as unknown, not passed. The reviewer authorized
one fresh read-only integration run; `cargo test -p agent-file-tools --test
integration subc_bridge_test::` then passed 101 tests (2 existing ignores), in
167.83 seconds. The final drain slice passed 7 tests. Rustfmt 1.10.0-stable
`cargo fmt --all -- --check` passed. Final scoped rust-analyzer diagnostics were
authoritative for all three Rust files: 0 errors and 0 warnings; structural
inspection remained partial: the index had not published a checkout call-graph
view, so dependency/dead-code relationships were not reported.

macOS: the complete `subc::` lib slice passed 336 tests (8 existing ignores), and
the complete bridge integration slice passed 101 tests (2 existing ignores), in
165.76 seconds. HOME was created first under `local-ignore/windows-subc-revision/`,
`AFT_STORAGE_DIR` was unset, and the existing Cargo/rustup toolchain homes were
retained. Cargo was 1.99.0. Native linking succeeded, but the linker warned that
`__eh_frame` exceeded the compact-unwind-table capacity; tests still ran and passed.

Three new mutation controls each failed only their named test, then were restored
from the staged live state with an empty working-tree `git diff --stat`:

1. Waiting for the executor before deadline handoff made
   `subc_bridge_bash_started_promotion_hands_off_before_transport_deadline` fail:
   `a started promotion must not hold its reply past the 2s test bound (25s transport budget)`.
2. Aborting the observer at handoff made
   `subc_bridge_bash_promote_panic_triggers_fatal_teardown` fail:
   `timed out waiting for late promote panic route goodbye`; exit was
   `ConnectionLost` rather than `ActorFatal`, although the forced panic executed.
3. Settling the late fatal event as another answer made
   `late_promotion_fatal_signals_teardown_without_settling_hold_twice` fail:
   `retain the sibling's hold`, with actual 0 instead of expected 1.

The revised comments received a read-only AI review across all three Rust files;
clarity findings were addressed. One remaining flag concerned the existing
project term "repeat breaker", whose reminder behavior is explained above.
Revision evidence is kept outside build output in
`local-ignore/windows-subc-revision/`; no logs or isolated homes are committed.
