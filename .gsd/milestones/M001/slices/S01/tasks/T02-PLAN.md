---
estimated_steps: 4
estimated_files: 3
---

# T02: Integration tests proving process reliability contract

**Slice:** S01 — Binary Scaffold & Persistent Protocol
**Milestone:** M001

## Description

Write integration tests that spawn the `aft` binary as a child process and exercise the operational reliability contract: 100+ sequential commands without failure, recovery from malformed JSON, structured errors for unknown commands, and clean shutdown on stdin EOF. These tests are the slice's demo — they prove R001 (persistent binary architecture) works under load.

## Steps

1. Create `tests/integration/mod.rs` and `tests/integration/protocol_test.rs`. Set up a helper function that builds and spawns the `aft` binary (`Command::new(cargo_bin("aft"))`) with piped stdin/stdout/stderr. Add a helper to send a JSON command and read the JSON response line.
2. Write `test_sequential_commands` — send 100+ sequential ping and echo commands with incrementing IDs, assert each response has the matching ID, correct `ok: true`, and expected data. Verify no responses are lost or out of order.
3. Write `test_malformed_json_recovery` — send a line of garbage text, assert error response with `id: "_parse_error"` and `ok: false`. Then send a valid ping command, assert it succeeds. This proves the process recovers and continues after bad input. Also test: empty line (should be skipped — send a valid command after and verify), and partial JSON.
4. Write `test_unknown_command` — send `{"id":"1","command":"nonexistent"}`, assert error response with `code: "unknown_command"`. Write `test_clean_shutdown` — send a few commands, close stdin, wait for process exit, assert exit code 0.

## Must-Haves

- [ ] 100+ sequential commands test passes
- [ ] Malformed JSON recovery test passes (error response, then successful next command)
- [ ] Unknown command returns structured error
- [ ] Clean shutdown on stdin EOF with exit code 0
- [ ] Helper spawns actual binary (not in-process) — true process-level test

## Verification

- `cargo test --test integration` passes all tests
- Test output shows 100+ commands were sent and verified
- No test uses `#[ignore]` — all run in CI

## Observability Impact

- **Test-time signals:** Each integration test logs the number of commands sent/received. The sequential test asserts ordering by ID, so a mismatch immediately identifies which command broke.
- **Future agent inspection:** Run `cargo test --test integration -- --nocapture` to see per-test stdout with command counts and timing. Test names map directly to reliability contract properties (sequential throughput, malformed recovery, unknown command errors, clean shutdown).
- **Failure visibility:** Test failures include the response JSON and expected values in the assertion message. The spawn helper captures stderr, so `[aft]` diagnostic lines are available for debugging failed tests.

## Inputs

- `src/main.rs` — the built binary (T01 output)
- `src/protocol.rs` — response format to assert against
- S01-RESEARCH.md — protocol contract: one JSON object per line, `id` echo, `_parse_error` sentinel

## Expected Output

- `tests/integration/mod.rs` — module declaration
- `tests/integration/protocol_test.rs` — four integration tests proving process reliability
