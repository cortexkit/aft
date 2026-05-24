---
id: T02
parent: S01
milestone: M001
provides:
  - 4 integration tests proving process reliability contract
  - AftProcess test helper for spawning and communicating with the binary
  - 120 sequential command throughput verification
  - malformed JSON recovery proof (8 scenarios)
key_files:
  - tests/integration/protocol_test.rs
key_decisions:
  - replaced per-call BufReader with persistent AftProcess struct holding a BufReader over stdout — prevents buffered data loss across sequential reads
  - split helper into send (expects response) and send_silent (no response expected, e.g. empty lines) to match protocol semantics
patterns_established:
  - AftProcess spawn/send/shutdown pattern for all future integration tests against the binary
  - stderr lifecycle assertion (startup + shutdown banners) as process health verification
observability_surfaces:
  - test stderr output shows command counts via eprintln! ([test] prefix)
  - run `cargo test --test integration -- --nocapture` to see per-test diagnostics
  - assertion messages include response JSON and index for pinpointing failures in sequential runs
duration: ~10min
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T02: Integration tests proving process reliability contract

**Rewrote integration tests with persistent AftProcess helper — 4 tests prove 120 sequential commands, 8 malformed recovery scenarios, unknown command errors, and clean shutdown with stderr lifecycle assertions.**

## What Happened

Replaced the T01 scaffold (6 simple tests with per-call BufReader) with 4 comprehensive tests using a new `AftProcess` struct that holds a persistent `BufReader<ChildStdout>`. The old approach created a fresh BufReader on each `send_and_receive` call, which would silently lose buffered data during 100+ sequential reads.

The four tests:

1. **test_sequential_commands** — sends 120 commands (ping/version/echo cycling by `i % 3`) with incrementing IDs, asserts each response has matching ID, correct `ok: true`, and expected payload. Verifies no responses lost or out of order. Process exits cleanly after.

2. **test_malformed_json_recovery** — 8 scenarios: garbage text → parse error then recovery; empty line → skipped, next command works; whitespace-only → skipped; partial/truncated JSON → parse error then recovery; valid JSON missing required fields → parse error then recovery. Each scenario sends a follow-up valid command to prove the process didn't crash.

3. **test_unknown_command** — sends two different unknown commands, asserts structured error with `code: "unknown_command"` and message containing the command name. Verifies process continues after errors.

4. **test_clean_shutdown** — sends 5 pings, closes stdin, waits for exit code 0. Asserts stderr contains `[aft] started` and `[aft] stdin closed, shutting down` banners.

## Verification

- `cargo test --test integration` — 4 tests pass
- `cargo test` — all 17 tests pass (13 unit + 4 integration)
- `cargo build` — 0 warnings
- No test uses `#[ignore]`
- Sequential test sends 120 commands (>100 required)

### Slice-level verification status

- ✅ `cargo build` succeeds with no warnings
- ✅ `cargo test` passes all unit tests (protocol serialization, error formatting, config defaults)
- ✅ 100+ sequential commands test — 120 commands sent and verified
- ✅ Malformed JSON recovery — 8 scenarios pass
- ✅ Clean shutdown on stdin EOF — exit code 0 with lifecycle banners

## Diagnostics

- Run `cargo test --test integration -- --nocapture` to see `[test]` prefixed command counts
- Sequential test failures report the command index and response JSON in assertion messages
- Malformed recovery test labels each scenario for easy identification
- Clean shutdown test dumps captured stderr content on assertion failure

## Deviations

- Replaced the 6-test scaffold from T01 with 4 comprehensive tests rather than extending in-place. The old per-call BufReader pattern was fundamentally broken for sequential reads, so a full rewrite was cleaner than patching.
- Plan said `tests/integration/mod.rs` — kept T01's `tests/integration/main.rs` (Cargo convention for multi-file integration test dirs).

## Known Issues

None.

## Files Created/Modified

- `tests/integration/protocol_test.rs` — rewrote with AftProcess helper and 4 comprehensive integration tests
- `.gsd/milestones/M001/slices/S01/tasks/T02-PLAN.md` — added Observability Impact section
