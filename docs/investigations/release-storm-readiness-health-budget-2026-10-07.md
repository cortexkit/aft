# Release storm readiness and budgeted health details

## Failure mechanism

The release storm failure at `subc_bridge_test.rs:1749` / `:1801` is not an AFT process exit. This harness runs AFT **inside the test process**, beside a fake-daemon thread (`run_subc_bridge_test_inner`, lines 1721–1782). At 90 seconds the fake daemon times out, panics, and drops its TCP connection. AFT then returns `SubcError::ConnectionLost`. A watchdog process exit would instead terminate the whole test binary, without reaching the second panic.

The fake daemon was stuck in its final readiness barrier. The old `health_has_ready_roots` required every expected root to appear in `health_report.metrics.roots`. That field is deliberately a diagnostic sample:

- `crates/aft/src/subc/health.rs:1261–1264` caps root detail and reserves a **12 KiB metrics budget** inside SUBC's 16 KiB envelope.
- `budget_health_metrics` at lines 1309–1335 pops root-detail rows and increments `root_details_omitted` until the report fits.
- `build_health_report_with_indexing` at lines 2240–2281 adds live indexing and headline fields before applying that budget.

Additional health fields pushed the four-root fixture over the budget. Three ready roots remained in the wire sample; the fourth was omitted, not unready. The readiness predicate could therefore never succeed, regardless of how long it waited.

## Captured Linux evidence

Reproduced from `29d53941ddfa38ff3e674b129f9f9308204d3326` in an Ubuntu 22.04 Docker container, with `CARGO_INCREMENTAL=0`, the release Cargo profile and default nextest profile. Tools matched CI: `rustc 1.99.0 (b940084d7 2026-09-28)`, `cargo 1.99.0 (5f94df478 2026-08-27)`, `cargo-nextest 0.9.138 (fc97e97bb 2026-06-21)`. The container used native Linux **aarch64**, rather than the CI runner's x86_64; no deadline, scale, embedding-delay or timing-hook override was used. Builds were limited to four jobs. No synthetic CPU load was generated.

The initial uninstrumented run failed at 90.109 seconds with exactly:

```text
subc_storm_rebinds_stay_live_under_build_and_tool_traffic fake daemon watchdog
subc mode exits cleanly: ConnectionLost
Summary [90.113s] 1 test run: 0 passed, 1 failed
```

For diagnosis only, the test temporarily initialized `env_logger` and printed its received readiness reports. These edits were removed before verification. Captured module log lines included:

```text
[2026-10-07T06:16:27Z INFO aft::logging] [ses_storm-r0-s0] index_event kind=build_ready plane=semantic build_id=b-6562-6 root=/tmp/.tmp5mKr3d key=10b61d3245437803 elapsed_ms=5029 files=2 chunks=2 skipped_rows=0
[2026-10-07T06:16:27Z INFO aft::logging] [ses_storm-r2-s0] index_event kind=build_ready plane=semantic build_id=b-6562-5 root=/tmp/.tmp22z5v8 key=e6bbfee5bb0ae084 elapsed_ms=5030 files=2 chunks=2 skipped_rows=0
```

The **last** readiness report, just before the fake-daemon timeout, still had:

```text
actor_count=4 root_count=4 warming_roots=0 root_details_omitted=1
roots: /tmp/.tmp22z5v8, /tmp/.tmpELY1pu, /tmp/.tmpEY10t0
all three root states=ready; all three search indexes=ready
semantic statuses=ready, disabled, disabled
frame_loop.last_tick_age_ms=160 frame_loop.wake_overdue=false
stall_watchdog.stall_count=0 active_stalls=0 captures=0
pending_binds.count=0 metrics_bytes=11467
```

There were 320 successful readiness health replies and **zero** `stall watchdog:` log lines. The omitted root was `/tmp/.tmp5mKr3d`, whose semantic build had already logged `build_ready` above. This exonerates both the 30-second restart watchdog and a 30-second frame-loop stall.

## Commit-boundary investigation

Each revision below was checked out in the isolated worker worktree and tested on Linux with:

```sh
cargo nextest run --cargo-profile release -p agent-file-tools --test integration -E 'test(subc_storm_rebinds_stay_live_under_build_and_tool_traffic)'
```

| Revision | Result |
| --- | --- |
| `9a8557df4` (parent of live indexing health) | pass, 11.327 s |
| `7e42a3b66` (live indexing health) | pass, 11.340 s |
| `37074f7e7` (privacy health fields) | pass, 11.341 s |
| `f94a13ab9` | pass, 11.514 s |
| `a5d4ebdcf` | pass, 11.279 s |
| `856ce1178` (immediately after `a5d4ebdcf`) | fail, 90.069 s |
| `b286699e8` | fail, 90.080 s |
| `29d53941d` | fail, 90.113 s |

The first observed adjacent pass/fail boundary is `a5d4ebdcf` → `856ce1178`. This does **not** implicate the new system-text digest algorithm: that change delegates the same SHA-256 operation to the protocol crate. The defect is a pre-existing completeness assumption in the test, exposed when the evolving health payload crosses its byte budget. The boundary is an observation on this Linux fixture, not a claim that all possible payload sizes first cross the budget at that commit. Widening the budget or deleting new health fields would merely move the failure to the next counter or larger storm scale.

## Correction and verification

The readiness barrier (`subc_storm_test.rs:2843–2872`) still polls health over the transport and still requires each root's state, search plane, and exact enabled/disabled semantic state. The omission-aware predicate is at lines 2883–2947; its three regression tests begin at lines 2983, 3011 and 3033. Only when health explicitly reports omitted detail does it supplement a missing row with that root's **actual live actor health snapshot**. A missing actor, scheduler contention, busy root, building/disabled search plane, or wrong semantic state remains unready. The new predicate tests cover these negative cases; they do not infer readiness from `warming_roots=0` or accept omissions blindly. No production behavior or deadline changes.

Ran the exact release-storm workflow command five consecutive times:

```sh
cargo nextest run --cargo-profile release -p agent-file-tools --test integration -E 'test(subc_storm)'
```

Each run passed all **18** tests (the existing 15 plus three predicate regressions), without retries. The formerly failing test took 11.282, 11.331, 11.340, 11.306 and 11.407 seconds respectively.

For mutation proof, staged the verified test file, confirmed an empty working-tree diff, and replaced the live omitted-root lookup in `wait_for_ready_health` with `None` (`NON-VACUITY BREAK`). Ran the same release slice with `--no-fail-fast` to observe every sibling. Exactly `subc_storm_test::subc_storm_rebinds_stay_live_under_build_and_tool_traffic` failed, again with the fake-daemon watchdog and `ConnectionLost` at 90 seconds; **all 17 other tests passed**, including all three new predicate tests. Restored from the index, touched the file, and confirmed an empty working-tree diff. The defect is therefore observed by the real storm, not just by synthetic predicate tests. After restoration, the full slice passed again: 18/18 tests, with the main storm at 11.685 seconds. `cargo fmt --all -- --check` and `git diff --check` also passed. Compilation retained an unrelated existing `VecDeque` unused-import warning in `cache_freshness.rs:4`.
