---
estimated_steps: 3
estimated_files: 3
---

# T02: Integration tests proving full plugin→binary stack

**Slice:** S06 — OpenCode Plugin Bridge
**Milestone:** M001

## Description

Write integration tests that prove the plugin actually works end-to-end: binary spawns correctly, bridge manages its lifecycle, tools round-trip commands through the full stack, and crash recovery works. Tests run against the `cargo build` binary.

## Steps

1. Configure Bun test runner in `package.json` (bun has built-in test support, no extra deps needed). Add a `pretest` script that runs `cargo build` in the parent directory to ensure the binary is fresh.

2. Write `src/__tests__/bridge.test.ts`:
   - Test: bridge spawns binary and ping returns pong
   - Test: multiple sequential requests return correct responses (ID correlation)
   - Test: bridge auto-restarts after binary crash (kill child process, next request succeeds)
   - Test: shutdown cleans up child process (no orphans)
   - Test: request to dead bridge after max retries rejects with error
   - All tests use the `target/debug/aft` binary path directly (bypasses resolver)

3. Write `src/__tests__/tools.test.ts`:
   - Test: outline tool on `tests/fixtures/sample.ts` returns entries array with known symbols
   - Test: write tool creates a temp file and returns `syntax_valid`
   - Test: edit_symbol replaces a function in a temp file, returns backup_id and syntax_valid
   - Test: undo restores the file after edit_symbol
   - Each test creates a BinaryBridge, calls the tool's execute with mock context, parses the JSON string response, and asserts on structure
   - Cleanup: temp files removed, bridge shut down in afterEach

## Must-Haves

- [ ] Bridge lifecycle tests pass: spawn, ping, crash-restart, shutdown
- [ ] At least 4 tool round-trips proven: outline, write, edit_symbol, undo
- [ ] Tests use real binary (cargo build), not mocks
- [ ] All tests clean up child processes and temp files

## Verification

- `cd opencode-plugin-aft && bun test` — all tests pass
- No orphan `aft` processes left after test run: `pgrep -f "target/debug/aft"` returns nothing after tests complete

## Observability Impact

- Signals added/changed: test output shows bridge spawn/shutdown lifecycle per test
- How a future agent inspects this: `bun test --verbose` shows individual test results
- Failure state exposed: failed tests include bridge stderr output and pending request state

## Inputs

- `opencode-plugin-aft/src/bridge.ts` — BinaryBridge class from T01
- `opencode-plugin-aft/src/tools/reading.ts` — outline tool from T01
- `opencode-plugin-aft/src/tools/editing.ts` — write, edit_symbol tools from T01
- `opencode-plugin-aft/src/tools/safety.ts` — undo tool from T01
- `tests/fixtures/sample.ts` — existing fixture with known symbols (greet, add, UserService, etc.)
- `target/debug/aft` — compiled binary from `cargo build`

## Expected Output

- `opencode-plugin-aft/src/__tests__/bridge.test.ts` — bridge lifecycle integration tests
- `opencode-plugin-aft/src/__tests__/tools.test.ts` — tool round-trip integration tests
- `opencode-plugin-aft/package.json` — updated with test scripts
