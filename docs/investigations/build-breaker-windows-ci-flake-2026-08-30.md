# Build-breaker Windows CI flake investigation — 2026-08-30

## Finding

`build_breaker::tests::durable_trip_agrees_across_navigation_inspect_and_health_snapshots` was an **S-class** specimen: scheduler time was charged as suspension age. It was not a completion-proxy race.

The test recorded the trip with `SystemTime::now()`, performed several SQLite opens and surface projections, then let health take a second independent wall-clock sample. Its final assertion required that health's age still be at most one second. A contended runner could delay the test between those samples while the durable breaker row, navigation response, inspect state, and health state were all correct. No asynchronous publish participates in this test, and the 15-second heartbeat-recency boundary is not exercised because the fixture directly attributes each death.

A deterministic red-first control moved the trip event two seconds behind the health sample without sleeping. The focused test failed at the former `health.suspended_domains[0].age_s <= 1` assertion, reproducing the loaded-runner interleaving without relying on host contention.

## Event-based rewrite

The test now uses one logical decision timeline:

1. Three attributed death events trip the breaker at deterministic timestamps.
2. The third write's returned decision is compared with a fresh read of the durable breaker row.
3. Navigation, inspect, and health project that same row at one injected snapshot timestamp.
4. Each surface asserts the exact domain, reason, death count, and five-second age it owns.
5. TTL expiry is advanced through the breaker's existing `*_at` clock seam.

Inspect detail rendering and health refresh now expose internal `*_at` paths; production entry points still supply the real clock. This removes the scheduler budget rather than widening it.

Mutation control replaced health's injected snapshot timestamp with the live wall clock. The focused regression failed because the fixed durable suspension had expired at that unrelated time, then passed after restoration. This proves the clock seam and authoritative-row read are load-bearing.

## Same-shape sweep

| Test | Classification |
|---|---|
| `three_zero_credit_deaths_trip_once_and_are_idempotent` | Not a candidate: injected event timestamps and direct durable-decision assertions. |
| `one_batch_per_death_still_trips_after_six_credited_attempts` | Not a candidate: injected event timestamps; no polling or elapsed budget. |
| `ttl_lifts_only_suspension_and_retains_death_history` | Not a candidate: TTL boundaries advance on a logical clock. |
| `root_and_domain_histories_are_isolated` | Not a candidate: synchronous durable reads on injected timestamps. |
| `burn_limit_trips_without_counter_credit` | Not a candidate: direct burn-threshold decision with an injected timestamp. |
| `durable_trip_agrees_across_navigation_inspect_and_health_snapshots` | **S, fixed:** independent wall-clock samples made scheduler delay look like state disagreement. |
| `sqlite_commit_barrier_child` | Not an assertion candidate: the 30-second sleep parks the producer after publishing its barrier event so the parent can kill it. |
| `kill_during_extract_and_reconciliation_commit_is_atomic_and_credit_uses_only_counter` | **S candidate, no rewrite here:** the fixture awaits the authoritative commit-barrier file, but its 10-second outer hang bound starts before child launch and can include scheduler delay. It is not P because the barrier is the producer event being tested. |
| `marker_recency_boundary_requires_exact_dead_process_after_fifteen_seconds` | Not a candidate: exact process evidence and both sides of the 15-second boundary use an injected clock. |
| `adopted_temp_marker_protection_is_load_bearing_and_covers_sqlite_sidecars` | Not a candidate: injected sweep time and explicit process evidence; its negative control proves marker protection. |
| `ambiguous_sweep_evidence_retains_once_through_seven_day_boundary` | Not a candidate: injected sweep times exercise both sides of the durable ambiguity boundary. |
| `ordinary_temp_floor_and_sixty_four_check_continuation_bound_each_pass` | Not a candidate: injected sweep time and durable continuation state; no polling budget. |
| `marker_clock_anomalies_are_ambiguous` | Not a candidate: deliberate future-heartbeat input is classified against an injected current time. |
| `heartbeat_interval_constant_preserves_three_sample_recency_window` | S-adjacent cleanup candidate, no rewrite here: the useful constant relation is deterministic, while the separate wall-clock-after-epoch sanity assertion proves no breaker decision and could be removed independently. |

## Verification

- `cargo test -p agent-file-tools --lib build_breaker::tests::durable_trip_agrees_across_navigation_inspect_and_health_snapshots -- --exact --nocapture` passed after the deterministic rewrite; the red-first and live-clock mutation controls both failed at the intended health assertions before restoration.
- `cargo test -p agent-file-tools --lib --quiet` passed all 2,875 selected unit tests (17 ignored).
- The full integration suite passed 1,618 tests and hit 19 pre-existing `callgraph_test` failures: this linked Mason worktree is correctly borrow-only, while those fixtures require creating a callgraph store. Re-running with the baseline module excluded passed all 1,584 selected tests (11 ignored).
- `cargo test --features test-hooks -p agent-file-tools --test watcher_integration --quiet -- --test-threads=1` passed all 18 tests.
- `cargo check --workspace --all-targets --target x86_64-pc-windows-gnu` passed.
- `cargo fmt --all -- --check` passed.
