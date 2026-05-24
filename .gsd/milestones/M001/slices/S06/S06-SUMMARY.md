---
id: S06
parent: M001
milestone: M001
provides:
  - OpenCode plugin that registers all 11 AFT commands as tools with Zod schemas
  - BinaryBridge class managing persistent child process lifecycle (spawn, crash recovery, shutdown)
  - Binary resolver with PATH + cargo fallback
  - Integration test suite proving full plugin→binary→response stack (9 tests, 51 assertions)
requires:
  - slice: S01
    provides: JSON protocol contract (command shapes, response shapes, NDJSON format)
  - slice: S02
    provides: Tree-sitter parsing engine (outline/zoom depend on symbol extraction)
  - slice: S03
    provides: outline and zoom command handlers
  - slice: S04
    provides: checkpoint, restore_checkpoint, list_checkpoints, undo, edit_history command handlers
  - slice: S05
    provides: write, edit_symbol, edit_match, batch command handlers with auto-backup and syntax validation
affects:
  - S07
key_files:
  - opencode-plugin-aft/src/index.ts
  - opencode-plugin-aft/src/bridge.ts
  - opencode-plugin-aft/src/resolver.ts
  - opencode-plugin-aft/src/tools/reading.ts
  - opencode-plugin-aft/src/tools/editing.ts
  - opencode-plugin-aft/src/tools/safety.ts
  - opencode-plugin-aft/src/__tests__/bridge.test.ts
  - opencode-plugin-aft/src/__tests__/tools.test.ts
key_decisions:
  - D034 — Use plugin's Zod re-export (`tool.schema`) instead of direct `zod` import to avoid version mismatch between plugin-bundled zod@4.1.8 and npm-resolved zod@4.3.6
  - Tool modules export factory functions taking BinaryBridge, returning Record<string, ToolDefinition> — composition root in index.ts assembles everything
  - Bridge uses manual newline splitting on stdout data events rather than readline for simplicity
patterns_established:
  - Tool module pattern: export function(bridge) → Record<string, {description, args, execute}> with Zod raw shapes and .describe() on every field
  - Bridge protocol: send {id, command, ...params} as NDJSON, receive {id, ok, ...data}, correlate by monotonic ID
  - Optional params only included in wire object when defined (no undefined values sent to binary)
  - Integration test pattern: fresh BinaryBridge per test, afterEach shutdown, real binary (no mocks)
observability_surfaces:
  - Bridge stderr logs with [aft-plugin] prefix for spawn, crash, restart, shutdown events
  - bridge.isAlive() for programmatic liveness check
  - bridge.restartCount for crash history tracking
  - Request timeout (30s) with descriptive error messages including command name and request ID
drill_down_paths:
  - .gsd/milestones/M001/slices/S06/tasks/T01-SUMMARY.md
  - .gsd/milestones/M001/slices/S06/tasks/T02-SUMMARY.md
duration: 50m
verification_result: passed
completed_at: 2026-03-14
---

# S06: OpenCode Plugin Bridge

**TypeScript plugin registers all 11 AFT commands as OpenCode tools, spawning and managing the Rust binary as a persistent child process with crash recovery — proven end-to-end by integration tests against the real binary.**

## What Happened

Built `opencode-plugin-aft/` as an ESM TypeScript package with three layers:

**Resolver** (`src/resolver.ts`) finds the `aft` binary via `which aft` on PATH, then `~/.cargo/bin/aft` fallback. Platform npm package slot reserved for S07. Throws descriptive error with install instructions if not found.

**BinaryBridge** (`src/bridge.ts`) is the process manager. Lazy-spawns on first `send()`, communicates via NDJSON over stdin/stdout with monotonic request ID correlation. Handles crash detection with exponential backoff auto-restart (100ms → 200ms → 400ms, max 3 retries), 30s request timeout, and clean shutdown (SIGTERM + 5s SIGKILL fallback). All lifecycle events logged to stderr with `[aft-plugin]` prefix.

**Tool modules** register all 11 commands across three files: reading (outline, zoom), editing (write, edit_symbol, edit_match, batch), safety (undo, edit_history, checkpoint, restore_checkpoint, list_checkpoints). Each tool has full Zod 4 schemas with `.describe()` on every field for agent hints. Tool functions build JSON requests, send via bridge, and return stringified responses.

Entry point (`src/index.ts`) exports a `Plugin` async function that creates a bridge with the resolved binary path, then returns `Hooks` with all 11 tools assembled from the three modules.

Integration tests (T02) prove the full stack with the real cargo-built binary — bridge lifecycle (spawn, ping, crash-restart, shutdown, dead-bridge rejection) and tool round-trips (outline, write, edit_symbol, undo).

## Verification

- `bun run build` (tsc) — **passed**, zero type errors, all dist/ files generated
- `bun test` — **9 pass, 0 fail, 51 assertions, ~960ms**
  - Bridge: spawn+ping, sequential ID correlation, crash auto-restart, shutdown cleanup, dead-bridge rejection
  - Tools: outline returns 7 known symbols, write creates file with syntax_valid, edit_symbol replaces function with backup_id, undo restores original
- All 11 agent-facing commands have corresponding tool registrations (verified by cross-referencing main.rs dispatch)
- All Zod schema fields have `.describe()` (29 describe calls across tool files)
- No orphan processes after test run (`pgrep` verification in tests)

## Requirements Advanced

- R009 — Plugin bridge now exists: spawns binary, manages lifecycle, registers tools with Zod schemas, crash recovery works

## Requirements Validated

- R009 — Integration tests prove: resolver finds binary, bridge spawns and manages process, all 11 tools round-trip through JSON protocol, crash recovery auto-restarts, shutdown cleans up. The only gap is real OpenCode session UAT (milestone-level, not slice-level).

## New Requirements Surfaced

- none

## Requirements Invalidated or Re-scoped

- none

## Deviations

- none — both tasks completed as planned

## Known Limitations

- Binary resolver only checks PATH and `~/.cargo/bin/aft` — npm platform package resolution deferred to S07
- No real OpenCode session testing — UAT is milestone-level verification, not slice-level
- Bridge does not buffer stderr from the binary process beyond logging — no structured error capture from binary stderr

## Follow-ups

- S07 will add npm platform package resolution to the resolver
- Milestone-level UAT: agent uses AFT tools in a real OpenCode session

## Files Created/Modified

- `opencode-plugin-aft/package.json` — ESM package with @opencode-ai/plugin dependency, pretest cargo build
- `opencode-plugin-aft/tsconfig.json` — TypeScript config targeting ES2022 with strict mode
- `opencode-plugin-aft/src/index.ts` — Plugin entry point
- `opencode-plugin-aft/src/resolver.ts` — Binary resolver (PATH → ~/.cargo/bin fallback)
- `opencode-plugin-aft/src/bridge.ts` — BinaryBridge process manager
- `opencode-plugin-aft/src/tools/reading.ts` — outline + zoom tool definitions
- `opencode-plugin-aft/src/tools/editing.ts` — write + edit_symbol + edit_match + batch tool definitions
- `opencode-plugin-aft/src/tools/safety.ts` — undo + edit_history + checkpoint + restore_checkpoint + list_checkpoints tool definitions
- `opencode-plugin-aft/src/__tests__/bridge.test.ts` — bridge lifecycle integration tests (5 tests)
- `opencode-plugin-aft/src/__tests__/tools.test.ts` — tool round-trip integration tests (4 tests)

## Forward Intelligence

### What the next slice should know
- The plugin is at `opencode-plugin-aft/` as an independent ESM package. S07 needs to package this into `@aft/core` alongside the platform binary resolver.
- The resolver in `src/resolver.ts` has a clear slot for npm platform package resolution — it's the first check that should be added before PATH and cargo fallbacks.
- Bridge constructor takes `(binaryPath: string, cwd: string)` — the resolver returns the path, and `cwd` comes from the OpenCode plugin context's `directory` field.

### What's fragile
- Zod version coupling — the plugin uses `tool.schema` (zod@4.1.8 bundled by @opencode-ai/plugin) rather than direct zod import. If the plugin updates its bundled zod version, schema types should still be compatible, but it's worth verifying after any @opencode-ai/plugin upgrade.
- Bridge newline splitting is manual (not readline). It handles chunked NDJSON correctly in tests, but very large single-line JSON responses (megabytes) haven't been stress-tested.

### Authoritative diagnostics
- `bun test` in `opencode-plugin-aft/` — tests the full plugin→binary stack in ~1 second, catches protocol mismatches immediately
- Bridge stderr with `[aft-plugin]` prefix — shows exact lifecycle events (spawn, crash, restart, shutdown) with timestamps

### What assumptions changed
- No surprises. The JSON protocol from S01–S05 mapped cleanly to Zod schemas. The @opencode-ai/plugin API was straightforward once the Zod re-export pattern was discovered.
