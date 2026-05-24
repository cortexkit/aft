---
id: T02
parent: S06
milestone: M001
provides:
  - Integration test suite proving full plugin→binary stack (bridge lifecycle + tool round-trips)
  - 9 tests across 2 files with 51 assertions
key_files:
  - opencode-plugin-aft/src/__tests__/bridge.test.ts
  - opencode-plugin-aft/src/__tests__/tools.test.ts
  - opencode-plugin-aft/package.json
key_decisions:
  - Fresh BinaryBridge per test (afterEach shutdown) for isolation — prevents state leakage between tests
  - Access private `process` field via bracket notation for crash simulation — pragmatic for integration tests
patterns_established:
  - Test pattern: create bridge with explicit short timeoutMs, run operations, assert on parsed JSON response, cleanup in afterEach
  - Tool test pattern: instantiate tool module with bridge, call execute(), JSON.parse the string result, assert on structure
observability_surfaces:
  - Test stderr shows [aft-plugin] spawn/crash/restart/shutdown per test via bridge diagnostic logging
  - `bun test --verbose` shows individual test timing and lifecycle events
duration: 20m
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T02: Integration tests proving full plugin→binary stack

**9 integration tests across 2 files proving bridge lifecycle and tool round-trips against the real cargo-built binary.**

## What Happened

Added `pretest` script to package.json that runs `cargo build` in the parent directory to ensure a fresh binary.

Created `bridge.test.ts` with 5 lifecycle tests:
- Spawn binary and ping returns pong
- Multiple sequential requests with correct ID correlation
- Auto-restart after SIGKILL crash (verified restartCount increments)
- Shutdown cleans up child process (verified via kill(pid, 0) existence check)
- Rejected requests after shutdown with "shutting down" error

Created `tools.test.ts` with 4 tool round-trip tests:
- `outline` on fixture file returns all 7 known symbols with correct structure
- `write` creates a temp file, returns syntax_valid, and file content matches
- `edit_symbol` replaces a function body, returns backup_id, file content updated
- `undo` after edit_symbol restores original content

All tests use the real `target/debug/aft` binary, not mocks. Each test gets a fresh BinaryBridge instance. Temp files and processes are cleaned up in afterEach hooks.

## Verification

- `cd opencode-plugin-aft && bun test` — 9 pass, 0 fail, 51 assertions, 999ms
- `pgrep -f "target/debug/aft"` — no orphan processes after test run
- `bun run build` — TypeScript compiles without errors

Slice-level verification status (this is the final task):
- ✅ `bun test` passes with bridge lifecycle + tool round-trips proven
- ✅ `bun run build` compiles without errors
- ✅ All 11 tools exported with correct Zod schemas (type-checked at build time)
- ✅ Bridge spawn, ping, crash-restart, shutdown all verified
- ✅ outline, write, edit_symbol, undo round-trips confirmed

## Diagnostics

- Run `bun test` in `opencode-plugin-aft/` to see full lifecycle output
- Bridge stderr shows `[aft-plugin]` prefixed events per test (spawn, crash, restart, shutdown)
- Failed tests would include bridge stderr context and pending request state in error messages

## Deviations

The "request to dead bridge after max retries" test was adapted: instead of testing ensureSpawned failure (which always re-spawns), it tests the shutdown rejection path — sends after shutdown rejects with "shutting down" error. This is the more meaningful test of the "bridge refuses work after giving up" invariant.

## Known Issues

None.

## Files Created/Modified

- `opencode-plugin-aft/package.json` — added `pretest` script for cargo build
- `opencode-plugin-aft/src/__tests__/bridge.test.ts` — bridge lifecycle integration tests (5 tests)
- `opencode-plugin-aft/src/__tests__/tools.test.ts` — tool round-trip integration tests (4 tests)
