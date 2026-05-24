---
estimated_steps: 7
estimated_files: 8
---

# T01: Plugin package, resolver, bridge, and all tool registrations

**Slice:** S06 — OpenCode Plugin Bridge
**Milestone:** M001

## Description

Build the complete OpenCode plugin package: package scaffold (ESM, Bun-compatible), binary resolver, BinaryBridge process manager, and all 11 agent-facing tool registrations with Zod 4 schemas. This is the full plugin — T02 adds tests to prove it works.

## Steps

1. Create `opencode-plugin-aft/package.json` with `@opencode-ai/plugin` and `zod` as dependencies, `"type": "module"`, build script using `bun build` or `tsc`. Create `tsconfig.json` targeting ESM with strict mode.

2. Implement `src/resolver.ts` — `findBinary(): string` function. Check: (a) `aft` on PATH via `child_process.execSync('which aft')`, (b) `~/.cargo/bin/aft` exists, (c) platform-specific npm package path (`@aft/{platform}` — stub for S07, won't resolve yet). Return first found path. Throw descriptive error with install instructions if none found.

3. Implement `src/bridge.ts` — `BinaryBridge` class:
   - Constructor takes `binaryPath: string` and `cwd: string`
   - `send(command: string, params: Record<string, unknown>): Promise<object>` — lazy-spawns binary on first call, generates monotonic request ID, writes NDJSON line to stdin, returns promise that resolves when response with matching ID arrives on stdout
   - Stdout parsing: buffer incoming data, split on `\n`, parse each complete line as JSON, route to pending request by ID
   - Pending request map: `Map<string, { resolve, reject, timer }>` — reject on timeout (30s), reject all on crash
   - Process lifecycle: `exit` event triggers crash detection, auto-restart with exponential backoff (100ms, 200ms, 400ms), max 3 retries before giving up
   - `shutdown(): Promise<void>` — kills child process, clears pending requests
   - `isAlive(): boolean` — process state check
   - Stderr capture: pipe to console.error with `[aft-plugin]` prefix for debugging

4. Implement `src/tools/reading.ts` — export tool definitions for `outline` and `zoom`:
   - outline: `{ file: z.string() }` → bridge.send("outline", { file })
   - zoom: `{ file: z.string(), symbol: z.string(), context_lines: z.number().optional(), scope: z.string().optional() }` → bridge.send("zoom", params)

5. Implement `src/tools/editing.ts` — export tool definitions for `write`, `edit_symbol`, `edit_match`, `batch`:
   - write: `{ file, content, create_dirs? }`
   - edit_symbol: `{ file, symbol, operation: z.enum([...]), content?, scope? }`
   - edit_match: `{ file, match, replacement, occurrence? }`
   - batch: `{ file, edits: z.array(...) }`

6. Implement `src/tools/safety.ts` — export tool definitions for `undo`, `edit_history`, `checkpoint`, `restore_checkpoint`, `list_checkpoints`:
   - undo: `{ file }`
   - edit_history: `{ file }`
   - checkpoint: `{ name, files? }`
   - restore_checkpoint: `{ name }`
   - list_checkpoints: `{}`

7. Implement `src/index.ts` — Plugin entry point:
   - Default export: `async (input: PluginInput) => Hooks`
   - Create BinaryBridge instance with `findBinary()` path and `input.directory` as cwd
   - Import tools from all three tool modules, spread into `Hooks.tool` map
   - Each tool's `execute` calls `bridge.send(command, args)` and returns `JSON.stringify(response)`

## Must-Haves

- [ ] Package compiles with `bun run build` or type-checks with `tsc --noEmit`
- [ ] Resolver finds binary via PATH and ~/.cargo/bin/aft fallbacks
- [ ] Bridge sends NDJSON, reads NDJSON, correlates by request ID
- [ ] Bridge handles crash detection with auto-restart and pending request rejection
- [ ] All 11 tools have Zod schemas with `.describe()` on every field
- [ ] Plugin entry exports correct `Plugin` type signature
- [ ] Tool execute functions return `Promise<string>` (stringified JSON)

## Observability Impact

- **Bridge lifecycle logging**: stderr messages with `[aft-plugin]` prefix for spawn, crash, restart, shutdown events — grep-able by future agents
- **`isAlive()` inspection**: programmatic health check on the bridge instance
- **Pending request rejection**: all in-flight requests get explicit error on crash, with restart count in error message
- **Request timeout**: 30s timeout per request prevents silent hangs — rejects with descriptive error
- **How to inspect**: check stderr output for `[aft-plugin]` lines; call `bridge.isAlive()` for liveness; `bridge.restartCount` for crash history

## Verification

- `cd opencode-plugin-aft && npx tsc --noEmit` — type-checks without errors
- Manual review: all 11 command names from `main.rs` dispatch (minus ping/version/echo/snapshot) have corresponding tool registrations

## Inputs

- `src/main.rs` — dispatch function showing all 14 commands (11 agent-facing + 3 internal)
- `tests/integration/edit_test.rs` — authoritative JSON request/response shapes for write, edit_symbol, edit_match, batch
- `tests/integration/commands_test.rs` — authoritative shapes for outline, zoom
- `tests/integration/safety_test.rs` — authoritative shapes for checkpoint, undo, edit_history
- S06-RESEARCH.md — `@opencode-ai/plugin` API: Plugin type, tool() helper, PluginInput context, Zod 4

## Expected Output

- `opencode-plugin-aft/package.json` — ESM package with dependencies
- `opencode-plugin-aft/tsconfig.json` — TypeScript config
- `opencode-plugin-aft/src/index.ts` — Plugin entry point
- `opencode-plugin-aft/src/resolver.ts` — Binary location resolver
- `opencode-plugin-aft/src/bridge.ts` — BinaryBridge process manager
- `opencode-plugin-aft/src/tools/reading.ts` — outline + zoom tool definitions
- `opencode-plugin-aft/src/tools/editing.ts` — write + edit_symbol + edit_match + batch tool definitions
- `opencode-plugin-aft/src/tools/safety.ts` — undo + edit_history + checkpoint + restore_checkpoint + list_checkpoints tool definitions
