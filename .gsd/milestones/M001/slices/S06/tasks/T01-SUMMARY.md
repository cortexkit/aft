---
id: T01
parent: S06
milestone: M001
provides:
  - Complete OpenCode plugin package with binary resolver, NDJSON bridge, and 11 tool registrations
  - Plugin entry point exporting correct Plugin type signature
key_files:
  - opencode-plugin-aft/src/index.ts
  - opencode-plugin-aft/src/bridge.ts
  - opencode-plugin-aft/src/resolver.ts
  - opencode-plugin-aft/src/tools/reading.ts
  - opencode-plugin-aft/src/tools/editing.ts
  - opencode-plugin-aft/src/tools/safety.ts
key_decisions:
  - Use `tool.schema` (plugin's re-exported Zod) instead of direct `zod` import to avoid version mismatch — plugin bundles zod@4.1.8, npm resolves zod@4.3.6 as top-level, and the type internals are structurally incompatible across minor versions
  - Tool functions use raw `args` shape pattern (not `z.object()`) per the plugin's `tool()` helper API contract
  - Bridge uses manual newline splitting on stdout data events rather than readline — simpler, no extra dependency, handles chunked NDJSON correctly
patterns_established:
  - Tool module pattern: export a function taking BinaryBridge, returning Record<string, ToolDefinition> — each tool has description, args (Zod raw shape with .describe()), execute returning Promise<string>
  - Bridge protocol: send `{ id, command, ...params }` as NDJSON, receive `{ id, ok, ...data }` — correlate by id
  - Optional params pattern: only include in the wire object when defined (avoids sending undefined values to the binary)
observability_surfaces:
  - Bridge stderr logs with [aft-plugin] prefix for spawn, crash, restart, shutdown events
  - bridge.isAlive() for programmatic liveness check
  - bridge.restartCount for crash history tracking
  - Request timeout (30s) with descriptive error messages including command name and request ID
duration: 30m
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T01: Plugin package, resolver, bridge, and all tool registrations

**Built the complete OpenCode plugin: ESM package scaffold, binary resolver with PATH + cargo fallback, NDJSON BinaryBridge with crash recovery, and all 11 agent-facing tool registrations with Zod schemas.**

## What Happened

Created `opencode-plugin-aft/` as an ESM TypeScript package depending on `@opencode-ai/plugin@^1.2.26`. The package has three layers:

1. **Resolver** (`src/resolver.ts`) — finds `aft` binary via `which aft` on PATH, then `~/.cargo/bin/aft` fallback. Platform npm package slot reserved for S07. Throws descriptive error with install instructions if not found.

2. **BinaryBridge** (`src/bridge.ts`) — process manager that lazy-spawns the binary on first `send()` call. Communicates via NDJSON (one JSON object per line on stdin/stdout). Features: monotonic request ID correlation, 30s request timeout, crash detection with exponential backoff auto-restart (100ms → 200ms → 400ms, max 3 retries), clean shutdown with SIGTERM + 5s SIGKILL fallback, stderr capture with `[aft-plugin]` prefix.

3. **Tool modules** — three files registering all 11 commands:
   - `tools/reading.ts`: outline, zoom (2 tools)
   - `tools/editing.ts`: write, edit_symbol, edit_match, batch (4 tools)
   - `tools/safety.ts`: undo, edit_history, checkpoint, restore_checkpoint, list_checkpoints (5 tools)

Each tool uses the plugin's `tool.schema` Zod export (not direct `zod` import) to avoid version mismatch. Every schema field has `.describe()` for agent hint generation.

The entry point (`src/index.ts`) is a `Plugin` async function that creates a bridge with the resolved binary path and the session's working directory, then returns a `Hooks` object with all 11 tools spread from the three modules.

## Verification

- `cd opencode-plugin-aft && npx tsc --noEmit` — **passed**, zero type errors
- `npx tsc` (full build) — **passed**, all `.js`, `.d.ts`, and sourcemap files generated in `dist/`
- Manual review: all 11 agent-facing commands from `main.rs` dispatch have corresponding tool registrations (batch, checkpoint, edit_history, edit_match, edit_symbol, list_checkpoints, outline, restore_checkpoint, undo, write, zoom)
- All schema fields have `.describe()` calls verified by grep count matching field count
- Plugin default export matches `Plugin` type signature (enforced by TypeScript)

### Slice-level verification (partial — T01 is task 1 of 2):
- ✅ `bun run build` (via `npx tsc`) compiles without errors
- ✅ All 11 tools exported with correct Zod schemas (type-checked at build time)
- ⏳ `bun test` — integration tests not yet created (T02)

## Diagnostics

- **Bridge lifecycle**: grep stderr for `[aft-plugin]` to see spawn/crash/restart/shutdown events
- **Health check**: call `bridge.isAlive()` or send `ping` command through any tool's bridge reference
- **Crash tracking**: `bridge.restartCount` shows how many auto-restarts have occurred
- **Request debugging**: timeout errors include command name and request ID for correlation

## Deviations

- Used `tool.schema` (plugin's Zod re-export) instead of direct `import { z } from "zod"` — discovered that `@opencode-ai/plugin` bundles `zod@4.1.8` internally while npm resolves `zod@4.3.6` at the top level, causing type incompatibility. This is the correct pattern per the plugin API.
- Removed `bun-types` from tsconfig types array — not installed as a dev dependency and not needed since we target Node types (Bun is runtime-compatible with Node APIs).

## Known Issues

- None

## Files Created/Modified

- `opencode-plugin-aft/package.json` — ESM package with @opencode-ai/plugin dependency
- `opencode-plugin-aft/tsconfig.json` — TypeScript config targeting ES2022 with strict mode
- `opencode-plugin-aft/src/index.ts` — Plugin entry point (Plugin type, creates bridge, registers all tools)
- `opencode-plugin-aft/src/resolver.ts` — Binary resolver (PATH → ~/.cargo/bin fallback)
- `opencode-plugin-aft/src/bridge.ts` — BinaryBridge process manager (NDJSON, crash recovery, timeout)
- `opencode-plugin-aft/src/tools/reading.ts` — outline + zoom tool definitions
- `opencode-plugin-aft/src/tools/editing.ts` — write + edit_symbol + edit_match + batch tool definitions
- `opencode-plugin-aft/src/tools/safety.ts` — undo + edit_history + checkpoint + restore_checkpoint + list_checkpoints tool definitions
- `.gsd/milestones/M001/slices/S06/tasks/T01-PLAN.md` — added Observability Impact section (pre-flight fix)
