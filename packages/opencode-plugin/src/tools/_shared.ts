/**
 * Shared helpers for plugin tool handlers.
 *
 * Every tool that talks to the Rust binary should use `callBridge()` instead
 * of calling `ctx.pool.getBridge(...).send(...)` directly. The helper:
 *
 *   1. Resolves the project root from `context.worktree ?? context.directory`
 *      (canonical path), so two tool calls in the same project always reach
 *      the same bridge even if the agent's cwd momentarily differs.
 *   2. Injects `session_id` from `context.sessionID` into every request so the
 *      Rust side can partition undo/checkpoint state per OpenCode session
 *      (issue #14 — one shared bridge per project, N sessions per bridge).
 *
 * Tools that specifically need the raw `BinaryBridge` (for example to call
 * `bridge.send()` multiple times with shared state) should use `bridgeFor()`
 * and still pass `session_id` explicitly.
 */

import { existsSync } from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import type {
  AftProjectTransport,
  AftTransportOptions,
  BridgeRequestOptions,
  ToolCallOptions,
  ToolCallResult,
} from "@cortexkit/aft-bridge";
import {
  adaptToolError,
  canonicalizeProjectRoot,
  decodeFileUrl,
  isBashTransportDeadError,
  timeoutForCommand,
} from "@cortexkit/aft-bridge";
import { tool } from "@opencode-ai/plugin";
import { ingestBgCompletions } from "../bg-notifications.js";
import { getSessionDirectory, getSessionDirectoryCached } from "../shared/session-directory.js";
import { markBridgeEnd, markBridgeStart } from "../tool-perf.js";
import type { PluginContext } from "../types.js";

const z = tool.schema;

/**
 * Optional integer with bounds.
 *
 * MUST be JSON-Schema-representable for OpenCode tool registration to
 * succeed: OpenCode wraps plugin args in a host `z.object()` and runs
 * `z.toJSONSchema(args, { io: "input" })` at session start. Any node
 * the host's Zod can't convert (e.g. `.transform()`, `.preprocess()`)
 * throws "Transforms cannot be represented in JSON Schema" and the
 * entire plugin fails to load. Keep this a plain schema — no
 * transforms. Empty-sentinel coercion (null/""/0 → undefined) belongs
 * in tool handlers via `coerceOptionalInt`, not in the schema.
 *
 * Regression guard: `tool-schemas-json-convertible.test.ts` runs
 * `z.toJSONSchema(z.object(args), { io: "input" })` on every plugin
 * tool. If anyone reintroduces a `.transform()` here it fails before
 * shipping.
 *
 * Return type is `any` to suppress TS2742 — Zod's inferred type leaks
 * `.bun/zod@...` paths that aren't portable across the host SDK and
 * our zod version. The type annotation has no runtime effect; the
 * contract test is the real invariant.
 */
// biome-ignore lint/suspicious/noExplicitAny: tool.schema's bounded-int return type isn't portably nameable; contract test enforces the actual invariant.
export const optionalInt = (min: number, max: number): any =>
  z.number().int().min(min).max(max).optional();

// Baseline bridge transport budget for bash-family control calls. The main
// orchestrated bash tool overrides this per request because Rust may hold the
// final response until the foreground wait window or hard-kill cap elapses.
// Keep this centralized so every bash-family RPC also keeps the shared bridge
// alive on transport timeout.
export const BASH_TRANSPORT_TIMEOUT_MS = 30_000;

// Re-exported from @cortexkit/aft-bridge — shared runtime coercion,
// formatting, and timeout tables live in the host-neutral bridge package.
export {
  coerceOptionalInt,
  formatBridgeErrorMessage,
  isEmptyParam,
  LONG_RUNNING_COMMAND_TIMEOUT_MS,
  timeoutForCommand,
} from "@cortexkit/aft-bridge";

/**
 * Minimum shape of the per-tool-call context provided by the OpenCode SDK.
 *
 * We only depend on a few fields so any similar context (including the Pi
 * plugin's `ExtensionContext`) can be passed through the same helpers once
 * they adopt session-aware calls.
 */
export interface ToolRuntime {
  /** Worktree root (preferred); falls back to `directory` when absent. */
  worktree?: string;
  /** Agent's working directory for this tool call. */
  directory: string;
  /** Opaque OpenCode session identifier. Missing in CLI tests / some hosts. */
  sessionID?: string;
}

/**
 * Canonicalize a directory path: strip trailing separators, resolve symlinks
 * via `realpath`, fall back to lexical resolution if the path doesn't exist.
 *
 * Used both for the canonical project-root key and for verifying the
 * session-stored directory before we use it for routing.
 */
function canonicalizeDirectory(dir: string): string {
  // Single shared canonicalizer: realpath + Windows verbatim/drive-case
  // normalization, matching the bridge pool's routing key and the RPC port
  // scope. Keeping a separate local realpath here (the old behavior) risked the
  // routing/permission key diverging from the bridge key on Windows.
  return canonicalizeProjectRoot(dir);
}

/**
 * Resolve the canonical project root for a runtime.
 *
 * Prefers `worktree` because that stays stable across OpenCode sessions in
 * the same project; falls back to `directory` when unavailable (standalone
 * CLI use, older hosts). Normalizes trailing slashes and resolves symlinks
 * so `/repo` and `/repo/` and `/Users/.../repo -> /Volumes/...` collapse to
 * the same key.
 *
 * NOTE: When the runtime carries a `sessionID` and we have a cached
 * session-stored directory for it (see `shared/session-directory.ts`), the
 * stored directory wins. This is the workaround for OpenCode's bug where
 * `ctx.directory` is set to `process.cwd()` rather than the resumed
 * session's actual project directory.
 */
export function projectRootFor(runtime: ToolRuntime): string {
  // Workaround: if OpenCode handed us a session ID and the session has a
  // resolved directory in our cache, use that. This survives `opencode -s`
  // launched from the wrong cwd.
  const cached = getSessionDirectoryCached(runtime.sessionID);
  if (typeof cached === "string" && cached.length > 0) {
    return canonicalizeDirectory(cached);
  }

  const raw = runtime.worktree ?? runtime.directory;
  return canonicalizeDirectory(raw);
}

/**
 * Warm the session-directory cache, then return the same project root that
 * callBridge()/bridgeFor() will use for this tool call. Permission checks must
 * resolve paths through this helper before dispatch so the path the user
 * approves is byte-for-byte the path the Rust bridge acts on.
 */
export async function resolveProjectRoot(
  ctx: PluginContext,
  runtime: ToolRuntime,
): Promise<string> {
  if (runtime.sessionID && getSessionDirectoryCached(runtime.sessionID) === undefined) {
    await getSessionDirectory(ctx.client, runtime.sessionID, runtime.directory);
  }
  return projectRootFor(runtime);
}

/**
 * Expand a leading `~` to the user's home directory. Node's `path.resolve`
 * treats `~` as a literal segment, so `~/foo` would otherwise resolve to
 * `<projectRoot>/~/foo`. Applied before any absolute/relative decision so all
 * file tools (read/write/edit/outline/zoom/delete/refactor/imports/safety)
 * accept `~/...` the same way the search tools already do.
 */
export function expandTilde(input: string): string {
  if (!input || !input.startsWith("~")) return input;
  if (input === "~") return os.homedir();
  if (input.startsWith("~/") || input.startsWith(`~${path.sep}`)) {
    return path.resolve(os.homedir(), input.slice(2));
  }
  // Leave `~user` forms untouched — we don't resolve other users' homes.
  return input;
}

// Shared with the Pi plugin and matched byte-for-byte by the Rust decoder
// (subc_translate.rs decode_file_url) so permission gates and the server
// judge the same filesystem target.
export { decodeFileUrl } from "@cortexkit/aft-bridge";

/** Resolve a user path exactly as a bridge request will: file: URLs decode
 * to local paths, `~` expands to home, absolute paths are preserved;
 * relative paths are rooted at the session/project root. */
export function resolvePathFromProjectRoot(projectRoot: string, target: string): string {
  const expanded = expandTilde(decodeFileUrl(target));
  return path.isAbsolute(expanded) ? expanded : path.resolve(projectRoot, expanded);
}

export async function resolvePathArg(
  ctx: PluginContext,
  runtime: ToolRuntime,
  target: string,
): Promise<string> {
  return resolvePathFromProjectRoot(await resolveProjectRoot(ctx, runtime), target);
}

/**
 * Get the BinaryBridge for the runtime's project root.
 *
 * Prefer `callBridge()` unless you need to send multiple requests yourself.
 *
 * This is synchronous and uses only the cached session directory. If the
 * cache is cold, it falls back to `runtime.directory` — `callBridge()`
 * eagerly warms the cache before calling this so the cache is hot for
 * subsequent calls in the same session.
 */
export function bridgeFor(ctx: PluginContext, runtime: ToolRuntime): AftProjectTransport {
  const projectRoot = projectRootFor(runtime);
  if (ctx.isProjectEnabled?.(projectRoot) === false) {
    throw new Error(`AFT disabled by config for ${projectRoot}`);
  }
  // A restored session can point at a reclaimed mason worktree (or any
  // deleted directory). Binding it would make the module configure, lease,
  // and warm indexes for a dead root — refuse before any transport work.
  if (!existsSync(projectRoot)) {
    throw new Error(`project directory no longer exists: ${projectRoot} (stale restored session?)`);
  }
  return ctx.pool.getBridge(projectRoot);
}

/**
 * Send a single command to the Rust binary with `session_id` injected.
 *
 * This is the canonical way for a tool handler to call AFT: the helper picks
 * the right bridge (project-keyed), attaches the session namespace from
 * `context.sessionID`, and returns whatever the binary responds.
 *
 * Before routing, it ensures the session-directory cache is warm so the
 * very first tool call on a resumed-from-wrong-cwd session still reaches
 * the correct project bridge. Subsequent calls hit the cache synchronously.
 *
 * The Rust side falls back to a shared default namespace when `session_id`
 * is absent (see `RawRequest::session()`), so hosts that don't expose a
 * session identifier still work — they just share undo/checkpoint state.
 */
export async function callBridge(
  ctx: PluginContext,
  runtime: ToolRuntime,
  command: string,
  params: Record<string, unknown> = {},
  options?: AftTransportOptions,
): Promise<Record<string, unknown>> {
  // Resolve the session's stored project directory once on first call —
  // OpenCode sets `runtime.directory = process.cwd()` even for resumed
  // sessions, so we can't trust it as the workspace root. Subsequent
  // calls in the same session hit the cache and skip the lookup.
  if (runtime.sessionID && getSessionDirectoryCached(runtime.sessionID) === undefined) {
    await getSessionDirectory(ctx.client, runtime.sessionID, runtime.directory);
  }

  const merged: Record<string, unknown> = { ...params };
  if (runtime.sessionID) {
    merged.session_id = runtime.sessionID;
  }
  // One canonical budget field: caller `transportTimeoutMs` (if any) wins,
  // otherwise the command budget. A caller-provided `timeoutMs` is consumed
  // as an override input but never re-emitted — bridge.ts resolves
  // `transportTimeoutMs ?? timeoutMs`, so emitting both fields lets the
  // legacy alias silently shadow the winner.
  const { timeoutMs: callerTimeoutMs, ...restOptions } = options ?? {};
  const timeoutMs = restOptions.transportTimeoutMs ?? callerTimeoutMs ?? timeoutForCommand(command);
  const sendOptions = {
    ...(timeoutMs !== undefined ? { transportTimeoutMs: timeoutMs } : {}),
    configureWarningClient: ctx.client,
    ...restOptions,
  };
  markBridgeStart();
  let response: Awaited<ReturnType<AftProjectTransport["send"]>>;
  try {
    response = await bridgeFor(ctx, runtime).send(
      command,
      merged,
      Object.keys(sendOptions).length > 0 ? sendOptions : undefined,
    );
  } catch (error) {
    // The break-glass gate must receive the original transport failure. In
    // particular, do not append recovery text before gate-off callers rethrow it.
    if (command === "bash" && isBashTransportDeadError(error)) throw error;
    throw adaptToolError(command, error);
  } finally {
    markBridgeEnd();
  }
  ingestBgCompletions(runtime.sessionID, response.bg_completions);
  return response;
}

/**
 * Dispatch one hoisted agent tool through the server-side `tool_call` command.
 *
 * The helper mirrors `callBridge()`: it warms the session-directory cache,
 * routes to the project-keyed bridge, applies the bare tool's timeout budget,
 * records the same perf marks, and ingests background-completion sidecars from
 * the raw response exactly once. The returned object is the full Rust response
 * plus the server-rendered `text` field.
 */
export async function callToolCall(
  ctx: PluginContext,
  runtime: ToolRuntime,
  name: string,
  rawArgs: Record<string, unknown> = {},
  options?: ToolCallOptions,
): Promise<ToolCallResult> {
  if (runtime.sessionID && getSessionDirectoryCached(runtime.sessionID) === undefined) {
    await getSessionDirectory(ctx.client, runtime.sessionID, runtime.directory);
  }

  const { timeoutMs: callerTimeoutMs, ...restOptions } = options ?? {};
  const timeoutMs = restOptions.transportTimeoutMs ?? callerTimeoutMs ?? timeoutForCommand(name);
  const sendOptions = {
    ...(timeoutMs !== undefined ? { transportTimeoutMs: timeoutMs } : {}),
    configureWarningClient: ctx.client,
    ...restOptions,
  };
  let response: Awaited<ReturnType<AftProjectTransport["toolCall"]>>;
  try {
    response = await bridgeFor(ctx, runtime).toolCall(
      runtime.sessionID,
      name,
      rawArgs,
      Object.keys(sendOptions).length > 0 ? sendOptions : undefined,
    );
  } catch (error) {
    throw adaptToolError(name, error);
  } finally {
    markBridgeEnd();
  }
  ingestBgCompletions(runtime.sessionID, response.bg_completions);
  return response;
}

/**
 * Send a bash-family command without restarting the shared bridge on transport
 * timeout. Bash has its own child-process timeout/watchdog handling; a late
 * transport response must not sacrifice the warm shared bridge and reject
 * unrelated sibling requests.
 */
export async function callBashBridge(
  ctx: PluginContext,
  runtime: ToolRuntime,
  command: string,
  params: Record<string, unknown> = {},
  options?: BridgeRequestOptions,
): Promise<Record<string, unknown>> {
  return await callBridge(ctx, runtime, command, params, {
    transportTimeoutMs: BASH_TRANSPORT_TIMEOUT_MS,
    ...options,
    keepBridgeOnTimeout: true,
  });
}
