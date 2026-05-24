# S06: OpenCode Plugin Bridge

**Goal:** All 11 AFT agent-facing commands are available as OpenCode tools via a TypeScript plugin that spawns and manages the Rust binary as a persistent child process.
**Demo:** Plugin loads in OpenCode, binary spawns on first tool call, agent calls `outline` on a real file and gets structured JSON back through the full plugin→bridge→binary→response stack.

## Must-Haves

- Plugin exports a `Plugin` async function returning `Hooks` with 11 tools registered
- `BinaryBridge` class spawns the `aft` binary as a persistent child process using `child_process.spawn`
- Bridge sends NDJSON on stdin, reads NDJSON on stdout with line-buffered parsing
- Bridge correlates requests/responses by ID using a pending-request map
- Bridge handles: lazy spawn on first call, crash detection with auto-restart (exponential backoff, max 3 retries), request timeout (30s default), clean shutdown
- `resolver.ts` finds binary via: PATH lookup → `~/.cargo/bin/aft` → error with install instructions (npm platform package slot reserved for S07)
- All 11 agent-facing tools have Zod 4 schemas matching the binary's JSON contract exactly
- Tool `execute` functions: build JSON request → `bridge.send()` → return stringified response
- Binary spawned with `cwd` set to `directory` from PluginInput context
- Every Zod field has `.describe()` for agent hint generation

## Proof Level

- This slice proves: integration (plugin spawns binary, tools round-trip through JSON protocol)
- Real runtime required: yes (binary must be built, plugin must spawn it)
- Human/UAT required: no (automated integration test sufficient — real OpenCode session is milestone-level UAT)

## Verification

- `cd opencode-plugin-aft && bun test` — integration tests pass, proving:
  - Resolver finds `aft` binary on PATH (cargo build puts it in target/debug)
  - Bridge spawns binary, sends ping, gets pong response
  - `outline` tool returns structured entries for a fixture file
  - `write` tool creates a file and returns syntax_valid
  - Bridge handles binary crash and auto-restarts
  - Clean shutdown kills the child process
- `bun run build` compiles TypeScript without errors
- All 11 tools are exported and have correct Zod schemas (type-checked at build time)

## Observability / Diagnostics

- Runtime signals: Bridge logs `[aft-plugin]` prefixed messages to stderr for spawn, crash, restart, shutdown events
- Inspection surfaces: Bridge exposes `isAlive()` method; `ping` command serves as health check
- Failure visibility: Bridge rejects pending requests with error on crash; restart count tracked per bridge instance
- Redaction constraints: none (no secrets flow through the bridge)

## Integration Closure

- Upstream surfaces consumed: JSON protocol contract from S01–S05 (command shapes in integration tests), binary built by `cargo build`
- New wiring introduced: plugin entry (`src/index.ts`) is the composition root — creates bridge, registers tools
- What remains before the milestone is truly usable end-to-end: S07 (binary distribution via npm)

## Tasks

- [x] **T01: Plugin package, resolver, bridge, and all tool registrations** `est:1h`
  - Why: This is the entire plugin — bridge manages the binary process, resolver finds it, tools map OpenCode calls to JSON commands. All three layers are tightly coupled and best built together.
  - Files: `opencode-plugin-aft/package.json`, `opencode-plugin-aft/tsconfig.json`, `opencode-plugin-aft/src/index.ts`, `opencode-plugin-aft/src/resolver.ts`, `opencode-plugin-aft/src/bridge.ts`, `opencode-plugin-aft/src/tools/reading.ts`, `opencode-plugin-aft/src/tools/editing.ts`, `opencode-plugin-aft/src/tools/safety.ts`
  - Do: (1) Create package.json with `@opencode-ai/plugin` and `zod` deps, ESM config. (2) `resolver.ts` — check PATH via `which`/`command -v`, then `~/.cargo/bin/aft`, return path or throw with instructions. (3) `bridge.ts` — BinaryBridge class: lazy spawn via child_process.spawn with stdio pipes, line-buffered stdout reader (split on `\n`), pending-request map keyed by monotonic ID, send() returns Promise that resolves when response with matching ID arrives, crash detection on `exit` event (reject all pending, auto-restart with backoff), request timeout (30s), shutdown() kills child. (4) `src/tools/reading.ts` — outline and zoom tool definitions. (5) `src/tools/editing.ts` — write, edit_symbol, edit_match, batch tool definitions. (6) `src/tools/safety.ts` — undo, edit_history, checkpoint, restore_checkpoint, list_checkpoints tool definitions. (7) `src/index.ts` — Plugin function: create bridge with resolver, return Hooks with all 11 tools spread from the three tool modules.
  - Verify: `cd opencode-plugin-aft && bun run build` compiles without errors
  - Done when: all 11 tools are registered with correct Zod schemas, bridge compiles, plugin entry exports correctly

- [x] **T02: Integration tests proving full plugin→binary stack** `est:45m`
  - Why: Plugin code compiling doesn't prove it works — need to verify binary spawns, tools round-trip, crash recovery works.
  - Files: `opencode-plugin-aft/src/__tests__/bridge.test.ts`, `opencode-plugin-aft/src/__tests__/tools.test.ts`
  - Do: (1) Add `bun test` config to package.json. (2) `bridge.test.ts` — test BinaryBridge directly: spawn with cargo-built binary path, ping round-trip, concurrent requests with ID correlation, crash detection + auto-restart (kill child, send new request), shutdown cleanup. (3) `tools.test.ts` — test tool execute functions end-to-end: outline on `tests/fixtures/sample.ts` returns entries, write creates a temp file with syntax_valid, edit_symbol on a temp file returns backup_id, undo restores. Each test creates a bridge pointed at `cargo build` binary, calls tool execute, asserts on response shape.
  - Verify: `cd opencode-plugin-aft && bun test` — all tests pass
  - Done when: Bridge lifecycle (spawn, ping, crash-restart, shutdown) and at least 4 tool round-trips (outline, write, edit_symbol, undo) proven end-to-end

## Files Likely Touched

- `opencode-plugin-aft/package.json`
- `opencode-plugin-aft/tsconfig.json`
- `opencode-plugin-aft/src/index.ts`
- `opencode-plugin-aft/src/resolver.ts`
- `opencode-plugin-aft/src/bridge.ts`
- `opencode-plugin-aft/src/tools/reading.ts`
- `opencode-plugin-aft/src/tools/editing.ts`
- `opencode-plugin-aft/src/tools/safety.ts`
- `opencode-plugin-aft/src/__tests__/bridge.test.ts`
- `opencode-plugin-aft/src/__tests__/tools.test.ts`
