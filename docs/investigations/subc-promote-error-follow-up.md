# Nonfatal subc promotion-error follow-up

Base: `bad57cf4e786e3d1ca9f89c1e1e3f01637b3c921` (`origin/train/v059-follow-up`).
The follow-up branch was created directly from that commit. CI run 38047395567
is an Actions run identifier, not a Git ref.

## Diagnosis and approved scope

The reported assertion was reproduced before editing with:

```sh
AFT_TEST_SUBC_BASH_PROMOTE_DELAY_MS=3000 cargo test -p agent-file-tools --test integration subc_bridge_bash_promote_failure_is_normal_tool_error
```

It failed at `tool_result_is_error(&frame)`, followed by `ConnectionLost`.
The deadline's successful task-id reply can beat the executor's forced error.
The result receiver deliberately cannot answer a request twice, so a late
nonfatal result was discarded without logging.

This reproduced case is **a test ordering assumption**, not an invented task id:

1. The foreground-wait path follows a successful running response and foreground
   registration in `submit_deferred_bash`.
2. Its deadline reply carries that registered task's id and independently starts
   `detach_held_bash_in_background`.
3. The forced-error hook replaces only the separate `submit_bash_promote` call;
   it does not fail the independent helper's `registry.promote` operation.
4. A failed promote does not kill or remove the task. Successful promotion
   metadata/notification persistence is a separate guarantee, discussed below.

The follow-up scope focuses on deterministic delivered-error coverage and
warnings for both discarded late nonfatal results and independent detach errors.
No reply deadline, handoff ownership, fatal teardown or persistence behavior was
changed in this follow-up.

## Changes and coverage boundaries

`subc_bridge_bash_promote_failure_is_normal_tool_error` now sends an actual
**delegated blocking wait** (`worker_session: true`, `wait: true`). Its real
worker wait cap triggers the same `submit_bash_promote`, but the ordinary
foreground reply timer is disabled for this mode. Therefore the promotion result
owns the reply even when the executor is delayed. The shell is held by a release
file instead of sleeping; it is released after the reply and must complete.
The test still requires the forced tool error and a successful sibling request,
proving that this nonfatal error does not tear down the module. The same 3-second
injected-delay reproduction command passes after the change.

Coverage is intentionally explicit:

- The delivered-error test above is **worker blocking-wait coverage**, not a claim
  that a plain foreground error always wins its deadline race.
- There is **no remaining end-to-end test that forces a promotion error to win
  before the deadline on the plain foreground path**. The old fixture did not
  guarantee that ordering; this change does not pretend otherwise.
- The new ordinary-path test
  `subc_bridge_bash_late_promote_failure_keeps_deadline_handoff_and_module_alive`
  proves the opposite ordering honestly: promotion starts and is delayed, the
  task-id reply wins within the existing bound, and the late nonfatal error must
  neither send a second response nor tear down the module.
- The existing 2-second started-promotion bound and independent delayed-panic
  teardown tests remain unchanged and passed the follow-up gates.

Both warning records name `task_id`, `session`, and `error`. Debug-quoted string
fields escape embedded newlines, keeping each message on one line. Warnings use
the repository's canonical `slog_warn!` emitter. Unit tests capture emitted
records with `capture_log_lines`, not strings searched from source.

The detach worker's existing body was factored into a private synchronous helper
called by the same `spawn_blocking` wrapper. This permits thread-local ENOSPC
injection and log capture around the real registry operation without changing
its asynchronous production scheduling.

## Separate finding: failed detach can lose its completion notice

**Not fixed in this delivery.** A caller can continue observing and managing the
task through `bash_status`, but its automatic completion notice can be absent.
The command itself does complete; it does not remain silently running forever.

Reproduce the real metadata-write failure and status observations with:

```sh
cargo test -p agent-file-tools --lib failed_background_handoff_logs_identity_and_keeps_status_observable -- --nocapture
```

The test spawns a real foreground command held by a file, registers it, and calls
the actual detach-worker body under `TaskIoFault::RunningEnospc`. It then invokes
the production `bash_status` handler while running, releases the command, and
observes its successful terminal status. The Linux observation was:

```text
failed-detach observation: running_status="running" terminal_status="completed" exit_code=0 notify_on_completion=false completion_delivered=true completion_notices=0
```

The code path explains the missing notice:

- `BgTaskRegistry::promote` in `bash_background/registry.rs` sets the background
  slot flag before updating task metadata. When `update_task_metadata_locked`
  fails, the task stays registered but its prior `notify_on_completion=false`
  metadata remains in memory.
- `mark_terminal` in `bash_background/persistence.rs` sets
  `completion_delivered = !notify_on_completion`.
- Terminal completion publication in `bash_background/registry.rs` returns
  without queueing a notice when `completion_delivered` is already true.

Thus a failed detach with no later successful promotion can finish with no
automatic completion notification. A later successful promotion can change that
state; it is not guaranteed. The regression asserts warning identity and status
visibility, but does **not** assert that missing notices are desired behavior.
The printed observation and code path identify a separate persistence/notification
follow-up rather than enshrining the absence as a contract.

## Verification

Tested code commit: `50ab751a79fdd89000ffb06495d982ccae3285f5`.
Cargo/rustc: 1.99.0. Rustfmt: 1.10.0-stable.

| Gate | Actual result |
| --- | --- |
| Linux `--lib subc::bash::` | 24 passed, 0 failed |
| Linux complete `subc_bridge_test::`, run 1 | 102 passed, 1 unrelated failure, 2 existing ignores; 182.71 s |
| Linux complete `subc_bridge_test::`, run 2 | 103 passed, 0 failed, 2 existing ignores; 181.90 s |
| Linux complete `subc_bridge_test::`, run 3 | 103 passed, 0 failed, 2 existing ignores; 175.19 s |
| macOS `--lib subc::bash::` | 24 passed, 0 failed |
| macOS complete `subc_bridge_test::` | 103 passed, 0 failed, 2 existing ignores; 174.66 s |
| Native Windows complete `subc_bridge_test::` | 103 passed, 0 failed, 2 existing ignores; 206.64 s |
| Windows broader `subc::` lib slice | 321 passed, 3 unrelated failures, 7 existing ignores |

All promotion-error, late-error, fatal-teardown and 2-second-bound tests passed
in all three Linux full-slice runs, on macOS, and in the single executed Windows
run. This does not claim an entirely green Linux run 1 or aggregate Windows gate.

Linux commands used `runon: "linux"`. macOS HOME was created first under
`local-ignore/promote-error-follow-up/home`, `AFT_STORAGE_DIR` was unset, and only
the installed Cargo/rustup toolchain homes were retained. Native macOS linking
reported the large compact-unwind-table warning; linking and tests succeeded.
`cargo fmt --all -- --check` passed. Sidekick reviewed both changed Rust files:
4 comment blocks, none flagged. Final scoped diagnostics were authoritative for
both Rust files: 0 errors and 0 warnings. Structural inspection remained partial
because the checkout call-graph view was unavailable.

The Windows invocation was:

```sh
scripts/windows-gate.sh --base bad57cf4e786e3d1ca9f89c1e1e3f01637b3c921
```

Two initial attempts were refused as busy before executing tests. With reviewer
approval, read-only `Wait-Process` calls waited for the shared supervisor; no
other worker's process or lock was changed. The one executed gate ran committed
code, passed storage preflight 1/1, and took 948.67 seconds. No maintenance lock
was reported.

Unrelated gate failures were left outside scope; none was separately reproduced
at the train base:

- Linux run 1: `subc_bridge_routebind_ack_is_prioritized_over_reliable_flood`,
  `crates/aft/tests/integration/subc_bridge_test.rs:6667`:
  `RouteBindAck waited behind 702 reliable Push frames after configure finished`.
  The same test passed Linux runs 2/3, macOS and Windows.
- Windows: `exec_remote_v1_bash_runs_through_real_route_without_invalid_selector`,
  `crates/aft/src/subc/bash_selector_tests.rs:90`: expected `"v1-bash-proof"`,
  received PowerShell's missing-`printf` diagnostic.
- Windows: `blocked_route_bind_capture_names_operation_resource_and_scheduler_holder`,
  `crates/aft/src/subc/stall_watchdog.rs:1145`: required
  `holder=crates/aft/src/executor/mod.rs:`; the captured Windows path contained
  `holder=crates\aft\src\executor\mod.rs:1358`.
- Windows: `scheduler_wait_context_names_live_holder_without_taking_scheduler_lock`,
  `crates/aft/src/subc/stall_watchdog.rs:850`: the same forward-slash assertion
  failed on the backslash Windows holder path.

## Mutation controls

Each control was marked as a temporary mutation after staging the live files,
then restored from that index. Working-tree `git diff --stat` was nonempty during
mutation and empty after restoration. No mutation was committed. Each selected
one-test run failed only its named test:

1. Turning off the worker request's blocking `wait`, with the 200 ms ordinary
   window and 3-second injected promotion delay, made
   `subc_bridge_bash_promote_failure_is_normal_tool_error` fail at
   `tool_result_is_error(&frame)` again.
2. Removing the discarded-error warning made
   `late_nonfatal_bash_error_after_handoff_logs_identity_without_another_reply`
   fail: `one canonical warning: []`, actual 0 instead of 1.
3. Removing the independent detach warning made
   `failed_background_handoff_logs_identity_and_keeps_status_observable` fail:
   `one handoff warning: []`, actual 0 instead of 1.
4. Removing the logging branch's fatal exclusion made
   `late_promotion_fatal_signals_teardown_without_settling_hold_twice` fail:
   `late fatal must still reach the completion queue: Elapsed(())`.

Evidence is retained outside build output under
`local-ignore/promote-error-follow-up/`; no logs, homes or caches are committed.
