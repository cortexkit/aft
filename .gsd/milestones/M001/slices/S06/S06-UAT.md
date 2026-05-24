# S06: OpenCode Plugin Bridge — UAT

**Milestone:** M001
**Written:** 2026-03-14

## UAT Type

- UAT mode: live-runtime
- Why this mode is sufficient: The plugin spawns a real binary process — correctness can only be verified by running the actual binary and observing real JSON round-trips. Artifact inspection alone cannot prove process lifecycle or protocol correctness.

## Preconditions

- Rust toolchain installed (`cargo` on PATH)
- `cargo build` has been run in the project root (or will be run by `bun test`'s pretest script)
- `bun` installed (v1.x)
- `cd opencode-plugin-aft && bun install` has been run
- The `target/debug/aft` binary exists and is functional

## Smoke Test

```bash
cd opencode-plugin-aft && bun test
```
All 9 tests pass. This confirms the plugin can find the binary, spawn it, communicate over NDJSON, and get structured responses back.

## Test Cases

### 1. Plugin builds without type errors

1. Run `cd opencode-plugin-aft && bun run build`
2. **Expected:** Exit code 0, `dist/` directory contains `.js`, `.d.ts`, and `.js.map` files for index, bridge, resolver, and all three tool modules

### 2. Binary resolver finds aft on PATH

1. Run `cargo build` in project root
2. Ensure `target/debug/aft` exists
3. Add `target/debug` to PATH (or verify `which aft` resolves)
4. In a Node/Bun REPL: `import { resolveAftBinary } from "./opencode-plugin-aft/dist/resolver.js"; console.log(await resolveAftBinary())`
5. **Expected:** Returns absolute path to the `aft` binary

### 3. Bridge spawns binary and responds to ping

1. Create a BinaryBridge with the resolved binary path and a valid cwd
2. Call `bridge.send({ command: "ping" })`
3. **Expected:** Response is `{ id: "...", ok: true, command: "pong" }`, `bridge.isAlive()` returns true

### 4. Outline tool returns structured symbols

1. Create a TypeScript file with at least 3 symbols (function, class, interface)
2. Call the `outline` tool's execute function with `{ file: "<path>" }`
3. Parse the JSON string result
4. **Expected:** `ok: true`, `entries` array contains objects with `name`, `kind`, `start_line`, `end_line`, `signature` fields. Each known symbol is present by name.

### 5. Write tool creates file with syntax validation

1. Call the `write` tool's execute function with `{ file: "/tmp/aft-uat-test.ts", content: "export function hello(): string { return 'world'; }\n" }`
2. Parse the JSON string result
3. Read `/tmp/aft-uat-test.ts` from disk
4. **Expected:** Response has `ok: true`, `syntax_valid: true`. File on disk matches the content sent.

### 6. Edit-symbol tool modifies a function by name

1. Write a file with `function greet() { return "hi"; }` using the write tool
2. Call `edit_symbol` with `{ file: "<path>", symbol: "greet", operation: "replace", content: "function greet() { return \"hello world\"; }" }`
3. Parse the response
4. Read the file from disk
5. **Expected:** Response has `ok: true`, `syntax_valid: true`, a `backup_id` string, and `new_range` with start/end lines. File content matches the replacement.

### 7. Undo restores previous file content

1. After test case 6, note the file path
2. Call `undo` with `{ file: "<path>" }`
3. Read the file from disk
4. **Expected:** Response has `ok: true`. File content is back to `function greet() { return "hi"; }`.

### 8. Bridge auto-restarts after binary crash

1. Spawn a bridge and verify it's alive (ping)
2. Kill the binary process (SIGKILL on the child PID)
3. Wait 200ms for restart
4. Send another ping
5. **Expected:** Second ping succeeds. `bridge.restartCount` is 1. stderr shows `[aft-plugin] Process exited` and `[aft-plugin] Auto-restart #1` messages.

### 9. Bridge shutdown cleans up child process

1. Spawn a bridge and note the child PID (from bridge internals or process list)
2. Call `bridge.shutdown()`
3. Check if the PID is still running (`kill -0 <pid>`)
4. **Expected:** PID no longer exists. No orphan `aft` processes. `bridge.isAlive()` returns false.

### 10. All 11 tools are registered with descriptions

1. Import the plugin entry point
2. Call the plugin function with a mock context
3. Inspect the returned hooks/tools
4. **Expected:** Exactly 11 tools: outline, zoom, write, edit_symbol, edit_match, batch, undo, edit_history, checkpoint, restore_checkpoint, list_checkpoints. Each has a description string and args schema.

## Edge Cases

### Binary not found

1. Set PATH to exclude the aft binary location and remove ~/.cargo/bin/aft
2. Call `resolveAftBinary()`
3. **Expected:** Throws an error with a message containing install instructions (mentions `cargo install` or `npm install`)

### Request timeout

1. Create a bridge with a very short timeoutMs (e.g., 1ms)
2. Send a command that requires binary processing
3. **Expected:** Promise rejects with a timeout error message that includes the command name and request ID

### Rapid sequential commands

1. Send 20 commands (mix of ping, outline, write) without awaiting between sends
2. Await all promises
3. **Expected:** All 20 responses arrive with correct ID correlation — no response goes to the wrong caller

### Malformed binary response

1. This is tested implicitly — the bridge's NDJSON parser silently drops lines that don't parse as JSON
2. **Expected:** Non-JSON stderr output from the binary does not crash the bridge or corrupt the pending-request map

## Failure Signals

- `bun test` has any failures — protocol contract broken between plugin and binary
- `bun run build` has type errors — Zod schema definitions don't match plugin API
- Orphan `aft` processes after test run (`pgrep -f "target/debug/aft"` finds processes) — shutdown not cleaning up
- Bridge restartCount exceeds expected in tests — binary crashing unexpectedly
- Tool execute returns non-JSON string — bridge response parsing broken

## Requirements Proved By This UAT

- R009 — Plugin bridge spawns binary, manages lifecycle (crash recovery, clean shutdown), registers all 11 tools with Zod schemas, round-trips JSON commands correctly
- R032 — All communication flows through JSON over stdin/stdout, verified by every tool round-trip test (no shell escaping anywhere in the stack)

## Not Proven By This UAT

- Real OpenCode session integration — plugin loading in OpenCode, agent using tools in conversation (milestone-level UAT)
- npm platform package binary resolution (S07)
- Cross-platform binary availability (S07)
- Long-running session stability (100+ sequential tool calls through the plugin — proven at the binary level in S01, but not through the full plugin stack)

## Notes for Tester

- The `pretest` script in package.json runs `cargo build` automatically before tests, so you don't need to build the binary separately
- Tests create temp files in `/tmp/aft-test-*` — these are cleaned up in afterEach hooks but may remain if a test crashes mid-execution
- Bridge stderr output is visible during tests as `[aft-plugin]` prefixed lines — this is expected diagnostic output, not errors
- The crash recovery test (case 8) intentionally kills the binary with SIGKILL — the `Process exited` log line is expected behavior
