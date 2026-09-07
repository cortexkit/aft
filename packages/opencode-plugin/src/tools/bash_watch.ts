import {
  type BridgeRequestOptions,
  coerceBoolean,
  isBridgeTransportTimeout,
  isTerminalStatus,
  sleep,
} from "@cortexkit/aft-bridge";
import type { ToolContext, ToolDefinition } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import {
  consumeBgCompletion,
  markBgCompletionDelivered,
  markExplicitControl,
  markTaskWaiting,
  unmarkExplicitControl,
  unmarkTaskWaiting,
} from "../bg-notifications.js";
import { resolveBashConfig } from "../config.js";
import { resolveIsSubagent } from "../shared/subagent-detect.js";
import { clearSyncWatchAbort, isSyncWatchAborted } from "../sync-watch-abort.js";
import type { PluginContext } from "../types.js";
import { callBashBridge, coerceOptionalInt, optionalInt, projectRootFor } from "./_shared.js";

const z = tool.schema;
const BASH_WAIT_POLL_INTERVAL_MS = 100;
const DEFAULT_BASH_STATUS_WAIT_TIMEOUT_MS = 30_000;
const REGEX_WAIT_SCAN_WINDOW_BYTES = 64 * 1024;

export type BashWaitPattern =
  | { kind: "substring"; value: string }
  | { kind: "regex"; source: string };
export type BashStatusWaited = {
  reason: "matched" | "exited" | "timeout" | "user_message" | "unavailable";
  elapsed_ms: number;
  match?: string;
  match_offset?: number;
  match_stream?: "stdout" | "stderr";
};

/**
 * Minimal snapshot used when a watch ends without ever reading task status
 * (the bridge stayed busy past our deadline). Keeps formatWatchResultText from
 * dereferencing an absent snapshot.
 */
function unavailableSnapshot(): Record<string, unknown> {
  return { status: "unknown" };
}
type BashStatusWithWait = Record<string, unknown> & { waited?: BashStatusWaited };
type OutputStream = "output" | "stderr";
type OutputCursor = { output: number; stderr: number };
type OutputScanChunk = { stream: OutputStream; text: string; baseOffset: number };
type OutputScanState = Record<OutputStream, { text: string; baseOffset: number }>;

function coerceConfiguredWatchTimeout(value: unknown, cap: number): number | undefined {
  try {
    return coerceOptionalInt(value, "timeoutMs", 1, cap);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    throw new Error(`${message} (bash.watch_sync_max_ms)`);
  }
}

export function createBashWatchTool(ctx: PluginContext): ToolDefinition {
  return {
    description:
      "Watch a background bash task. Sync waits are for a short remaining wait on a task (default 30s, max `bash.watch_sync_max_ms`, 120s by default); for anything longer end the turn on `bash({background:true})` and let the completion reminder wake you, or use `bash({wait:true})` when the result is needed before anything else. The user can interrupt anytime; the wait auto-converts to an async notification. Async (background:true, requires pattern) registers a non-blocking notification and returns immediately — use when you have parallel work or want to end your turn. Never loop bash_status to wait.",
    args: {
      taskId: z.string().describe("Background task ID returned by bash({ background: true })."),
      pattern: z
        .union([z.string(), z.object({ regex: z.string() })])
        .optional()
        .describe(
          "Substring or regex pattern. Optional in sync mode; required with background:true. Sync substring watches keep only the overlap tail needed for boundary matches; sync regex watches use a 64 KB rolling output window.",
        ),
      background: z
        .boolean()
        .optional()
        .describe(
          "When true, register an async watch and return immediately. Defaults to false (sync wait).",
        ),
      timeoutMs: optionalInt(1, 1800000).describe(
        "Sync-only timeout in milliseconds. Default 30000; max `bash.watch_sync_max_ms` (120000 by default).",
      ),
      once: z
        .boolean()
        .optional()
        .describe("Async-only. Defaults true; false keeps the watch sticky until task exit."),
    },
    execute: async (args, context) => {
      const taskId = args.taskId as string;
      // Coerce at the boundary: stringified background must enable async mode (coerceBoolean).
      const requestedAsync = coerceBoolean(args.background);
      const waitFor = parseWaitPattern(args.pattern);
      const bashCfg = resolveBashConfig(ctx.config);
      const isSubagent = await resolveIsSubagent(ctx.client, context.sessionID, context.directory);
      const subagentForcedSync = requestedAsync && isSubagent && !bashCfg.subagent_background;
      const asyncMode = requestedAsync && !subagentForcedSync;

      if (asyncMode) {
        if (!waitFor) {
          throw new Error(
            "invalid_request: Use auto-reminder; bash_watch without pattern in async mode is redundant",
          );
        }
        const notifyParams: Record<string, unknown> = {
          task_id: taskId,
          once: coerceBoolean(args.once, true),
        };
        if (waitFor.kind === "regex") notifyParams.regex = waitFor.source;
        else notifyParams.pattern = waitFor.value;
        markExplicitControl(context.sessionID, taskId, false);
        let registered: Record<string, unknown>;
        try {
          registered = await callBashBridge(ctx, context, "bash_notify", notifyParams);
        } catch (err) {
          unmarkExplicitControl(context.sessionID, taskId);
          throw err;
        }
        if (registered.success === false) {
          unmarkExplicitControl(context.sessionID, taskId);
          const code = String(registered.code ?? "invalid_request");
          const message = String(registered.message ?? "bash_notify failed");
          if (code === "too_many_watches") throw new Error(`invalid_request: ${message}`);
          throw new Error(`${code}: ${message}`);
        }
        const metadata = (context as { metadata?: (data: Record<string, unknown>) => void })
          .metadata;
        metadata?.({ taskId, registered: true, watchId: registered.watch_id });
        return `Watch registered: ${registered.watch_id} on task ${taskId}\nA notification will fire when the pattern matches or the task exits.`;
      }

      const syncWaitCap = bashCfg.watch_sync_max_ms;
      const effectiveWaitMs = subagentForcedSync
        ? syncWaitCap
        : Math.min(
            coerceConfiguredWatchTimeout(args.timeoutMs, syncWaitCap) ??
              DEFAULT_BASH_STATUS_WAIT_TIMEOUT_MS,
            syncWaitCap,
          );
      const data = await waitForBashStatus(
        ctx,
        context,
        taskId,
        undefined,
        waitFor,
        effectiveWaitMs,
      );
      const waited = data.waited;
      // User-message abort: the sync wait was interrupted because the user
      // sent a message. Auto-register the equivalent async watch so the
      // notification still arrives, and return text explaining the conversion.
      if (waited?.reason === "user_message") {
        const convertedText = await convertToAsyncWatchOnAbort(
          ctx,
          context,
          taskId,
          waitFor,
          coerceBoolean(args.once, true),
        );
        const metadata = (context as { metadata?: (data: Record<string, unknown>) => void })
          .metadata;
        metadata?.({ taskId, status: data.status, waited, convertedToAsync: true });
        return convertedText;
      }
      const metadata = (context as { metadata?: (data: Record<string, unknown>) => void }).metadata;
      if (waited) metadata?.({ taskId, status: data.status, waited });
      return formatWatchResultText(taskId, data, waited);
    },
  };
}

/**
 * When a sync bash_watch wait is aborted because the user sent a message,
 * auto-register the equivalent async watch so the notification still arrives.
 * If the original sync watch had a pattern, register an async watch with the
 * same pattern. If it had no pattern (exit-only), the auto-reminder system
 * already handles exit notifications, so just return the conversion message.
 */
async function convertToAsyncWatchOnAbort(
  ctx: PluginContext,
  context: ToolContext,
  taskId: string,
  waitFor: BashWaitPattern | undefined,
  once: boolean,
): Promise<string> {
  // No pattern: the auto-reminder system already handles exit notifications
  // for background tasks, so no explicit watch registration is needed.
  if (!waitFor) {
    return (
      `Sync watch for task ${taskId} was interrupted because you sent a message. ` +
      `The task is still running in the background. A completion reminder will be ` +
      `delivered automatically when the task exits; don't poll bash_status.`
    );
  }
  // Register the equivalent async watch so the pattern/exit notification
  // still arrives. Reuse the same registration path as the explicit async mode.
  const notifyParams: Record<string, unknown> = {
    task_id: taskId,
    once,
  };
  if (waitFor.kind === "regex") notifyParams.regex = waitFor.source;
  else notifyParams.pattern = waitFor.value;
  markExplicitControl(context.sessionID, taskId, false);
  try {
    const registered = await callBashBridge(ctx, context, "bash_notify", notifyParams);
    if (registered.success === false) {
      unmarkExplicitControl(context.sessionID, taskId);
      // Registration failed — fall back to the auto-reminder path.
      return (
        `Sync watch for task ${taskId} was interrupted because you sent a message. ` +
        `Auto-registering an async watch failed (${String(registered.message ?? "unknown error")}). ` +
        `The task is still running in the background. A completion reminder will be ` +
        `delivered automatically when the task exits.`
      );
    }
    return (
      `Sync watch for task ${taskId} was interrupted because you sent a message. ` +
      `The wait has been converted to an async watch (${registered.watch_id}). ` +
      `A notification will fire when the pattern matches or the task exits.`
    );
  } catch (err) {
    unmarkExplicitControl(context.sessionID, taskId);
    // Registration failed — fall back to the auto-reminder path.
    return (
      `Sync watch for task ${taskId} was interrupted because you sent a message. ` +
      `Auto-registering an async watch failed (${err instanceof Error ? err.message : String(err)}). ` +
      `The task is still running in the background. A completion reminder will be ` +
      `delivered automatically when the task exits.`
    );
  }
}

function formatWatchResultText(
  taskId: string,
  data: Record<string, unknown>,
  waited: BashStatusWaited | undefined,
): string {
  const status = data.status as string;
  const exit = typeof data.exit_code === "number" ? ` (exit ${data.exit_code})` : "";
  const dur =
    typeof data.duration_ms === "number" ? ` ${Math.round(data.duration_ms / 1000)}s` : "";
  let text = `Task ${taskId}: ${status}${exit}${dur}`;
  if (waited) {
    if (waited.reason === "matched") {
      const stream = waited.match_stream ? ` in ${waited.match_stream}` : "";
      text += `\nWaited ${waited.elapsed_ms}ms; matched ${JSON.stringify(waited.match ?? "")}${stream} at offset ${waited.match_offset ?? 0}.`;
    } else if (waited.reason === "timeout") {
      text += `\nWaited ${waited.elapsed_ms}ms; timeout reached without match.`;
    } else if (waited.reason === "unavailable") {
      text += `\nWaited ${waited.elapsed_ms}ms; the bridge was busy, so task state is unknown. Do not poll; let the task's completion notification wake the session, or use one bash_status snapshot on the next normal tool call.`;
    } else {
      const stat = String(data.status ?? "unknown");
      const e = typeof data.exit_code === "number" ? `, exit ${data.exit_code}` : "";
      text += `\nWaited ${waited.elapsed_ms}ms; task exited (${stat}${e}).`;
    }
  }
  const preview = data.output_preview as string | undefined;
  if (preview && status !== "running") {
    text += `\n${preview}`;
  }
  return text;
}

async function bashStatusSnapshot(
  ctx: PluginContext,
  runtime: ToolContext,
  taskId: string,
  outputMode: string | undefined,
  cursor?: OutputCursor,
  options?: BridgeRequestOptions,
): Promise<Record<string, unknown>> {
  const data = await callBashBridge(
    ctx,
    runtime,
    "bash_status",
    {
      task_id: taskId,
      output_mode: outputMode,
      output_offset: cursor?.output,
      stderr_offset: cursor?.stderr,
    },
    options,
  );
  if (data.success === false)
    throw new Error((data.message as string | undefined) ?? "bash_status failed");
  return data;
}

export async function waitForBashStatus(
  ctx: PluginContext,
  runtime: ToolContext,
  taskId: string,
  outputMode: string | undefined,
  waitFor: BashWaitPattern | undefined,
  effectiveWaitMs: number,
): Promise<BashStatusWithWait> {
  const startedAt = Date.now();
  const deadline = startedAt + effectiveWaitMs;
  let spillCursor: OutputCursor = { output: 0, stderr: 0 };
  const scanState: OutputScanState = {
    output: { text: "", baseOffset: 0 },
    stderr: { text: "", baseOffset: 0 },
  };
  const bridgeOptions: BridgeRequestOptions = {};
  if (waitFor?.kind === "regex") {
    await validateWaitRegex(ctx, runtime, waitFor);
  }
  // Clear any stale abort flag from a previous turn so it doesn't insta-abort
  // this new wait.
  clearSyncWatchAbort(runtime.sessionID);
  markTaskWaiting(runtime.sessionID, taskId);
  let sawTerminal = false;
  let lastData: Record<string, unknown> | undefined;
  try {
    while (true) {
      let data: Record<string, unknown>;
      try {
        data = await bashStatusSnapshot(
          ctx,
          runtime,
          taskId,
          outputMode,
          waitFor ? spillCursor : undefined,
          bridgeOptions,
        );
      } catch (err) {
        // A single poll's transport timeout means the bridge is *busy*, not
        // that the task failed — the bridge is kept warm (keepBridgeOnTimeout).
        // Don't abort the whole watch (which surfaces a red "Failed" to the
        // user every poll); honor abort/deadline and otherwise retry. A
        // genuine non-timeout error still propagates.
        if (!isBridgeTransportTimeout(err)) throw err;
        if (isSyncWatchAborted(runtime.sessionID)) {
          return withWaited(lastData ?? unavailableSnapshot(), {
            reason: "user_message",
            elapsed_ms: Date.now() - startedAt,
          });
        }
        if (Date.now() >= deadline) {
          return withWaited(lastData ?? unavailableSnapshot(), {
            reason: "unavailable",
            elapsed_ms: Date.now() - startedAt,
          });
        }
        await sleep(Math.min(BASH_WAIT_POLL_INTERVAL_MS, Math.max(0, deadline - Date.now())));
        continue;
      }
      lastData = data;
      const terminal = isTerminalStatus(data.status);
      if (waitFor) {
        const scan = await readNewTaskOutput(data, spillCursor);
        if (scan) {
          spillCursor = scan.nextCursor;
          // Independent buffers prevent a stdout suffix and stderr prefix from
          // becoming a fabricated match. When both streams match in one poll,
          // readNewTaskOutput's stdout-first chunk order is the tie-breaker.
          for (const chunk of scan.chunks) {
            const state = scanState[chunk.stream];
            if (state.text.length === 0) state.baseOffset = chunk.baseOffset;
            state.text += chunk.text;
            if (waitFor.kind === "regex") {
              const trimmed = trimWaitScanBuffer(state.text, state.baseOffset, waitFor);
              state.text = trimmed.text;
              state.baseOffset = trimmed.baseOffset;
            }
            const match = await findWaitMatch(ctx, runtime, state.text, waitFor);
            if (match) {
              if (terminal) {
                sawTerminal = true;
                consumeBgCompletion(runtime.sessionID, taskId);
                await markBgCompletionDelivered(
                  { ctx, directory: projectRootFor(runtime), sessionID: runtime.sessionID },
                  taskId,
                );
              }
              const matchStream: "stdout" | "stderr" | undefined =
                data.mode === "pty" ? undefined : chunk.stream === "output" ? "stdout" : "stderr";
              return withWaited(data, {
                reason: "matched",
                elapsed_ms: Date.now() - startedAt,
                match: match.text,
                match_offset: state.baseOffset + match.byteOffset,
                match_stream: matchStream,
              });
            }
            if (waitFor.kind === "substring") {
              const trimmed = trimWaitScanBuffer(state.text, state.baseOffset, waitFor);
              state.text = trimmed.text;
              state.baseOffset = trimmed.baseOffset;
            }
          }
        }
      }
      if (terminal) {
        sawTerminal = true;
        consumeBgCompletion(runtime.sessionID, taskId);
        await markBgCompletionDelivered(
          { ctx, directory: projectRootFor(runtime), sessionID: runtime.sessionID },
          taskId,
        );
        return withWaited(data, { reason: "exited", elapsed_ms: Date.now() - startedAt });
      }
      // User-message abort: if the user sent a message while we were
      // blocking, convert this sync wait to an async watch so the agent's
      // turn ends promptly. The match/exit checks above win over abort —
      // if the task already matched or exited in this iteration, we return
      // that result instead of aborting.
      if (isSyncWatchAborted(runtime.sessionID)) {
        return withWaited(data, { reason: "user_message", elapsed_ms: Date.now() - startedAt });
      }
      if (Date.now() >= deadline) {
        return withWaited(data, { reason: "timeout", elapsed_ms: Date.now() - startedAt });
      }
      await sleep(Math.min(BASH_WAIT_POLL_INTERVAL_MS, Math.max(0, deadline - Date.now())));
    }
  } finally {
    if (!sawTerminal) unmarkTaskWaiting(runtime.sessionID, taskId);
  }
}

async function readNewTaskOutput(
  data: Record<string, unknown>,
  cursor: OutputCursor,
): Promise<{ chunks: OutputScanChunk[]; nextCursor: OutputCursor } | undefined> {
  const stdoutBytes =
    typeof data.output_chunk_base64 === "string"
      ? Buffer.from(data.output_chunk_base64, "base64")
      : Buffer.alloc(0);
  const stderrBytes =
    typeof data.stderr_chunk_base64 === "string"
      ? Buffer.from(data.stderr_chunk_base64, "base64")
      : Buffer.alloc(0);
  if (stdoutBytes.length + stderrBytes.length === 0) return undefined;
  const chunks: OutputScanChunk[] = [];
  if (stdoutBytes.length > 0) {
    chunks.push({
      stream: "output",
      text: stdoutBytes.toString("utf8"),
      baseOffset: cursor.output,
    });
  }
  if (stderrBytes.length > 0) {
    chunks.push({
      stream: "stderr",
      text: stderrBytes.toString("utf8"),
      baseOffset: cursor.stderr,
    });
  }
  return {
    chunks,
    nextCursor: {
      output:
        typeof data.output_next_offset === "number"
          ? data.output_next_offset
          : cursor.output + stdoutBytes.length,
      stderr:
        typeof data.stderr_next_offset === "number"
          ? data.stderr_next_offset
          : cursor.stderr + stderrBytes.length,
    },
  };
}

export function parseWaitPattern(value: unknown): BashWaitPattern | undefined {
  if (typeof value === "string") return { kind: "substring", value };
  if (isRegexWaitObject(value)) return { kind: "regex", source: value.regex };
  return undefined;
}
function isRegexWaitObject(value: unknown): value is { regex: string } {
  return (
    typeof value === "object" &&
    value !== null &&
    "regex" in value &&
    typeof (value as { regex?: unknown }).regex === "string"
  );
}

type WaitMatch = { text: string; byteOffset: number };

async function validateWaitRegex(
  ctx: PluginContext,
  runtime: ToolContext,
  pattern: Extract<BashWaitPattern, { kind: "regex" }>,
): Promise<void> {
  await matchRegexWithBridge(ctx, runtime, pattern.source, "");
}

async function findWaitMatch(
  ctx: PluginContext,
  runtime: ToolContext,
  text: string,
  pattern: BashWaitPattern,
): Promise<WaitMatch | undefined> {
  if (pattern.kind === "substring") {
    const index = text.indexOf(pattern.value);
    return index >= 0
      ? { text: pattern.value, byteOffset: Buffer.byteLength(text.slice(0, index), "utf8") }
      : undefined;
  }
  return await matchRegexWithBridge(ctx, runtime, pattern.source, text);
}

async function matchRegexWithBridge(
  ctx: PluginContext,
  runtime: ToolContext,
  pattern: string,
  text: string,
): Promise<WaitMatch | undefined> {
  const result = await callBashBridge(ctx, runtime, "bash_regex_match", { pattern, text });
  if (result.success === false) {
    const code = String(result.code ?? "invalid_request");
    const message = String(result.message ?? "bash_regex_match failed");
    if (code === "invalid_regex") throw new Error(`invalid_request: invalid_regex: ${message}`);
    throw new Error(`${code}: ${message}`);
  }
  if (result.matched !== true) return undefined;
  return {
    text: typeof result.match_text === "string" ? result.match_text : "",
    byteOffset: coerceMatchOffset(result.match_offset),
  };
}

function coerceMatchOffset(value: unknown): number {
  const offset = typeof value === "number" ? value : Number(value ?? 0);
  return Number.isFinite(offset) && offset >= 0 ? offset : 0;
}

function trimWaitScanBuffer(
  text: string,
  baseOffset: number,
  pattern: BashWaitPattern,
): { text: string; baseOffset: number } {
  const keepFrom =
    pattern.kind === "substring"
      ? substringKeepStart(text, pattern.value)
      : regexKeepStart(text, REGEX_WAIT_SCAN_WINDOW_BYTES);
  if (keepFrom <= 0) return { text, baseOffset };

  return {
    text: text.slice(keepFrom),
    baseOffset: baseOffset + Buffer.byteLength(text.slice(0, keepFrom), "utf8"),
  };
}

function substringKeepStart(text: string, pattern: string): number {
  const keepChars = Math.max(0, pattern.length - 1);
  return text.length > keepChars ? text.length - keepChars : 0;
}

function regexKeepStart(text: string, maxBytes: number): number {
  if (Buffer.byteLength(text, "utf8") <= maxBytes) return 0;

  let low = 0;
  let high = text.length;
  while (low < high) {
    const mid = Math.floor((low + high) / 2);
    if (Buffer.byteLength(text.slice(mid), "utf8") > maxBytes) {
      low = mid + 1;
    } else {
      high = mid;
    }
  }
  return low;
}

export function __trimWaitScanBufferForTests(
  text: string,
  baseOffset: number,
  pattern: BashWaitPattern,
): { text: string; baseOffset: number } {
  return trimWaitScanBuffer(text, baseOffset, pattern);
}

function withWaited(data: Record<string, unknown>, waited: BashStatusWaited): BashStatusWithWait {
  return { ...data, waited };
}
