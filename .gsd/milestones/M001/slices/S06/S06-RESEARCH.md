# S06: OpenCode Plugin Bridge — Research

**Date:** 2026-03-14

## Summary

S06 delivers the TypeScript plugin that makes all AFT commands available as OpenCode tools. The plugin API is minimal and well-documented: a `Plugin` async function returning a `Hooks` object with a `tool` map. Each tool is defined with `tool()` helper — Zod schema for args, string-returning `execute` function, plus a description. The plugin package (`@opencode-ai/plugin@1.2.26`) depends on Zod 4 (`zod@4.1.8`), and `tool.schema` is literally the Zod `z` export.

The plugin needs three components: (1) a `BinaryBridge` class that spawns the `aft` binary as a persistent child process, sends JSON commands over stdin, reads JSON responses from stdout, handles crash detection and auto-restart; (2) a `resolver` module that locates the binary (npm platform package → PATH → cargo install location); (3) tool registration files with Zod schemas for all 14 commands. The Rust binary's JSON protocol is well-established — all command shapes are documented in integration tests and handler source.

Key architectural choice: the plugin is a **thin bridge**. All logic lives in the Rust binary. Each tool's `execute` function constructs a JSON request, sends it via the bridge, and returns the JSON response as a string. The plugin adds zero business logic — it's purely schema validation + process management.

## Recommendation

Build the plugin as a standalone npm package in `opencode-plugin-aft/` with three clear layers:

1. **`src/resolver.ts`** — Binary finder. Check: (a) platform-specific npm package `@aft/{platform}` (for S07), (b) `aft` on PATH via `which`, (c) `~/.cargo/bin/aft`. Return the first found path or throw with install instructions. For S06, only PATH and cargo locations matter (npm packages are S07).

2. **`src/bridge.ts`** — `BinaryBridge` class. Spawns `aft` binary via `child_process.spawn` with `{ stdio: ['pipe', 'pipe', 'pipe'] }`. Sends NDJSON on stdin, reads NDJSON from stdout using a line-buffered reader. Maintains a pending-request map keyed by request ID. Health check via `ping` command. Auto-restart on process exit (with exponential backoff and max retries). Lazy initialization — spawn on first tool call, not at plugin load time.

3. **`src/tools/`** — One file per command group. Each exports tool definitions with Zod schemas matching the binary's JSON params exactly. All tools share the same `execute` pattern: build JSON request → `bridge.send()` → return response string.

Use Bun runtime (OpenCode runs plugins with Bun). Package as ESM with `type: "module"`. Depend on `@opencode-ai/plugin` for types and `tool` helper.

## Don't Hand-Roll

| Problem | Existing Solution | Why Use It |
|---------|------------------|------------|
| Zod schema validation | `tool.schema` (Zod 4 via `@opencode-ai/plugin`) | Plugin API expects Zod schemas — `tool.schema` is the canonical Zod export |
| Plugin type contract | `Plugin`, `ToolContext`, `tool()` from `@opencode-ai/plugin` | Type safety and compatibility guaranteed |
| Line-buffered stdin/stdout | Node.js `readline` or manual `\n` splitting on `data` events | Standard pattern for NDJSON over child process pipes |

## Existing Code and Patterns

- `src/main.rs` dispatch function — authoritative list of all 14 commands (ping, version, echo, outline, zoom, undo, edit_history, checkpoint, restore_checkpoint, list_checkpoints, write, edit_symbol, edit_match, batch, snapshot)
- `src/protocol.rs` — `RawRequest` shape: `{ id, command, ...params }` with optional `lsp_hints`. `Response` shape: `{ id, ok, ...data }` with flattened data
- `tests/integration/edit_test.rs` — authoritative request/response JSON shapes for all edit commands
- `tests/integration/commands_test.rs` — authoritative shapes for outline and zoom
- `tests/integration/safety_test.rs` — authoritative shapes for checkpoint, undo, edit_history
- `@opencode-ai/plugin` (`tool.d.ts`) — `tool()` function signature: `{ description: string, args: ZodRawShape, execute(args, context): Promise<string> }`. `tool.schema` = Zod `z` export
- `@opencode-ai/plugin` (`index.d.ts`) — `Plugin` type: `(input: PluginInput) => Promise<Hooks>`. `PluginInput` has `{ client, project, directory, worktree, serverUrl, $ }`. `Hooks.tool` is `{ [key: string]: ToolDefinition }`

## Constraints

- **Plugin execute must return `Promise<string>`** — all tool output must be stringified JSON. The bridge should return `JSON.stringify(response)` from each `execute` call
- **Zod 4 (not 3)** — `@opencode-ai/plugin@1.2.26` depends on `zod@4.1.8`. Zod 4 is mostly API-compatible for basic types (string, number, boolean, object, array, enum, optional, describe) but some edge cases changed (see Zod 4 changelog)
- **Bun runtime** — OpenCode runs plugins with Bun. `child_process.spawn` works in Bun. `Bun.$` shell API is available via `ctx.$` but not needed for binary spawn
- **ESM only** — package must use `"type": "module"` with ESM imports
- **Plugin loading** — can be loaded from `.opencode/plugins/` directory, `~/.config/opencode/plugins/`, or from npm via `opencode.json` `"plugin"` array. For development, local file path works: `"plugin": ["./opencode-plugin-aft/src/index.ts"]`
- **ToolContext provides `directory` and `worktree`** — use `directory` as the binary's working directory. File paths in tool calls should be relative to `directory` or absolute
- **Binary is persistent** — one bridge instance per plugin lifecycle. Bridge spawns binary once, reuses across all tool calls. Must handle binary crash gracefully (auto-restart, pending request rejection)
- **Request IDs** — plugin must generate unique IDs for each request. Binary echoes the ID back in the response. Use monotonic counter or UUID

## Common Pitfalls

- **stdout line buffering** — Node.js `child_process` stdout may deliver data in chunks that don't align with newlines. Must buffer and split on `\n` before parsing JSON. Using `readline.createInterface` on the child's stdout handles this
- **stderr capture** — Binary writes diagnostics to stderr with `[aft]` prefix. Plugin should capture stderr for debugging but not parse it as command responses. Stderr and stdout are independent streams
- **Concurrent requests** — The binary processes requests sequentially (single-threaded). Plugin can queue requests but must not assume parallel processing. Request-response matching by ID handles out-of-order scenarios (though the binary responds in order)
- **Process lifecycle** — Must handle: (1) binary not found at startup, (2) binary crashes mid-session, (3) clean shutdown when plugin unloads, (4) binary hangs (timeout on requests). Lazy spawn + auto-restart covers most cases
- **File path resolution** — `context.directory` is the session's working directory. Tool args with `file` params may be relative. Bridge should resolve relative paths against `context.directory` before sending to the binary. Or let the binary handle it (binary uses the path as-is, which means relative paths resolve against the binary's cwd — must set the binary's cwd to `context.directory`)
- **Binary cwd** — Spawn the binary with `cwd: directory` from the plugin context. This ensures relative file paths in commands resolve correctly. The `directory` comes from the `PluginInput` context at plugin initialization, or can be passed per-call from `ToolContext`
- **Zod `.describe()` for agent hints** — Every arg should have a `.describe()` call so the LLM knows what it's for. These descriptions become part of the tool's parameter schema sent to the model

## Open Risks

- **Plugin API stability** — `@opencode-ai/plugin` is at version 1.2.26 with frequent releases. The core `Plugin` type and `tool()` helper are stable, but hooks and context fields may evolve. Pin the dependency version
- **Bun vs Node child_process behavior** — Bun's `child_process` is Node-compatible but there may be subtle differences in stream buffering or signal handling. Test in actual Bun runtime
- **Binary cwd vs per-call directory** — `PluginInput` gives `directory` at plugin init time, but `ToolContext` also has `directory` per tool call. If the user changes session directory mid-conversation, the bridge's cwd may be stale. Consider: either spawn the binary without a fixed cwd and have tools send absolute paths, or accept that binary cwd is fixed to the initial directory and resolve paths in the plugin before sending
- **`snapshot` test command** — D027 exposes this in dispatch for testing. Should we register it as a tool? Probably not — it's internal. But leaving it unregistered means agents can't manually trigger backups (auto-backup on mutations covers the normal case)
- **14 commands → 11 agent-facing tools** — `ping`, `version`, `echo` are internal/diagnostic. `snapshot` is test-only. That leaves 11 real tools: outline, zoom, write, edit_symbol, edit_match, batch, undo, edit_history, checkpoint, restore_checkpoint, list_checkpoints. The plugin should register only these 11 as OpenCode tools

## Command Contract Summary

All commands use NDJSON: `{ "id": "...", "command": "...", ...params }` → `{ "id": "...", "ok": bool, ...data }`

### Agent-Facing Commands (11 tools to register)

| Command | Required Params | Optional Params | Success Response Shape |
|---------|----------------|-----------------|----------------------|
| `outline` | `file: string` | — | `{ entries: OutlineEntry[] }` |
| `zoom` | `file: string`, `symbol: string` | `context_lines: number`, `scope: string` | `{ name, kind, range, content, context_before, context_after, annotations: { calls_out, called_by } }` |
| `write` | `file: string`, `content: string` | `create_dirs: boolean` | `{ file, created, syntax_valid, backup_id? }` |
| `edit_symbol` | `file: string`, `symbol: string`, `operation: enum` | `content: string`, `scope: string` | `{ symbol, operation, range, syntax_valid, backup_id }` or `{ code: "ambiguous_symbol", candidates }` |
| `edit_match` | `file: string`, `match: string`, `replacement: string` | `occurrence: number` | `{ replacements, syntax_valid, backup_id }` or `{ code: "ambiguous_match", occurrences }` |
| `batch` | `file: string`, `edits: array` | — | `{ file, edits_applied, syntax_valid, backup_id }` |
| `undo` | `file: string` | — | `{ file, backup_id, restored }` |
| `edit_history` | `file: string` | — | `{ entries: [{ backup_id, timestamp, description }] }` |
| `checkpoint` | `name: string` | `files: string[]` | `{ name, file_count }` |
| `restore_checkpoint` | `name: string` | — | `{ name, files_restored }` |
| `list_checkpoints` | — | — | `{ checkpoints: [{ name, file_count, created_at }] }` |

### Edit operations enum for `edit_symbol`
`"replace"` | `"delete"` | `"insert_before"` | `"insert_after"`

### Batch edit item shapes
- Match-replace: `{ match: string, replacement: string }`
- Line-range: `{ line_start: number, line_end: number, content: string }`

## Skills Discovered

| Technology | Skill | Status |
|------------|-------|--------|
| OpenCode plugins | `igorwarzocha/opencode-workflows@create-opencode-plugin` | available (72 installs) — may have useful scaffolding patterns |
| OpenCode | `s-hiraoku/synapse-a2a@opencode-expert` | available (170 installs) — general OpenCode expertise, not plugin-specific |

## Sources

- OpenCode Plugin API types and tool helper (source: `@opencode-ai/plugin@1.2.26` npm package, inspected `dist/tool.d.ts`, `dist/index.d.ts`, `dist/tool.js`)
- Plugin loading and configuration (source: [OpenCode Plugins docs](https://opencode.ai/docs/plugins/index) via Context7)
- Plugin context and hooks (source: [OpenCode GitHub](https://github.com/anomalyco/opencode) via Context7)
- Zod 4 compatibility (source: [Zod 4 changelog](https://zod.dev/v4/changelog) via Context7)
- Command JSON contracts (source: `tests/integration/edit_test.rs`, `commands_test.rs`, `safety_test.rs` — local codebase)
- Command parameter extraction (source: `src/commands/*.rs` handler implementations — local codebase)
