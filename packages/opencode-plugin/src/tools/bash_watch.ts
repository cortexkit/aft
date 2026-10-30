import {
  BashOutputPageDecoder,
  type BridgeRequestOptions,
  coerceBoolean,
  formatWatchWaited,
  interruptedWatchTail,
  isBridgeTransportTimeout,
  isTerminalStatus,
  maxWatchTimeoutMs,
  REGEX_WINDOW_SCAN_INCOMPLETE_TEXT,
  resolveWatchTimeoutMs,
  TERMINAL_SCAN_INCOMPLETE_TEXT,
  taskKillDeadlineText,
  taskKillDeadlineWithinHandoffMargin,
  WATCH_EACH_LINE_NEEDS_BACKGROUND,
  WATCH_SYNC_DEFAULTS_DESCRIPTION,
  WATCH_TIMEOUT_PARAM_DESCRIPTION,
  WATCH_UNAVAILABLE_GIVE_UP_MS,
  type WatchCallerRole,
  WORKER_KEEP_WAITING,
  watchClock,
  watchPollDelayMs,
  watchTimeoutSteer,
  watchUnavailableSteer,
  workerWatchStillRunning,
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
import { resolveSubagentStatus } from "../shared/subagent-detect.js";
import { clearSyncWatchAbort, isSyncWatchAborted } from "../sync-watch-abort.js";
import type { PluginContext } from "../types.js";
import {
  allowV2WatchWake,
  awaitV2ExecutionSettlement,
  trackV2Watch,
  v2SubagentStopped,
} from "../v2-session-lifecycle.js";
import { callBashBridge, coerceOptionalInt, optionalInt, projectRootFor } from "./_shared.js";
import { bashCompanionRegistered } from "./bash.js";
import { metadataUpdater } from "./metadata.js";

const z = tool.schema;
const REGEX_WAIT_SCAN_WINDOW_BYTES = 64 * 1024;
/**
 * Most bytes a held regex match may keep in the scan buffer: the window plus
 * one 64 KiB bash_status page. The engine's own watch never holds more than
 * that either (it cuts to its window after every page). A held match that
 * would need more is let go, the buffer is trimmed as usual, and the trimmed
 * bytes mark the scan incomplete.
 */
const REGEX_WAIT_HELD_MATCH_CAP_BYTES = 2 * REGEX_WAIT_SCAN_WINDOW_BYTES;

export type BashWaitPattern =
  | { kind: "substring"; value: string }
  | { kind: "regex"; source: string };
export type BashStatusWaited = {
  reason: "matched" | "exited" | "timeout" | "user_message" | "unavailable" | "aborted";
  /** Real time the watch held the call, measured on a monotonic clock. */
  elapsed_ms: number;
  /** Longest time this watch was allowed to hold the call; absent when it had no deadline. */
  limit_ms?: number;
  match?: string;
  match_offset?: number;
  match_stream?: "stdout" | "stderr";
  /** The task exited without a match, but a regex scan dropped older output
   * from its window, so the non-match is not a complete answer. */
  scan_incomplete?: boolean;
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
/**
 * One stream's scan buffer. `context` is the character just before a trimmed
 * regex window: it is sent with the text but the search starts after it, so
 * the trimmed start never looks like the start of the output.
 */
type OutputScanState = Record<OutputStream, { text: string; baseOffset: number; context: string }>;

function coerceConfiguredWatchTimeout(
  value: unknown,
  role: WatchCallerRole,
  cap: number,
): number | undefined {
  try {
    return coerceOptionalInt(value, "timeoutMs", 1, maxWatchTimeoutMs(role, cap));
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    // Only a primary's timeout is bounded by the configured cap.
    if (role === "worker") throw error;
    throw new Error(`${message} (bash.watch_sync_max_ms)`);
  }
}

/**
 * The bash_watch tool description. Every plugin registration uses the
 * `primary` wording, which covers both roles. The subc module catalog's
 * `worker` preset uses the `worker` wording: a delegated worker is never woken
 * by a completion reminder or an async notification once its turn ends, so it
 * is told only how to wait. The polling warning names bash_status only when
 * the model can call it.
 */
export function bashWatchDescription(role: WatchCallerRole, statusRegistered: boolean): string {
  const noPolling = statusRegistered ? " Never loop bash_status to wait." : "";
  if (role === "worker") {
    return `Watch a background bash task and wait until it exits or prints a pattern. ${WATCH_SYNC_DEFAULTS_DESCRIPTION}. A background task never wakes you, so wait on it before you report a result. ${WORKER_KEEP_WAITING}${noPolling}`;
  }
  return `Watch a background bash task. ${WATCH_SYNC_DEFAULTS_DESCRIPTION}. In a main session sync waits are for a short remaining wait on a task; for anything longer end the turn on \`bash({background:true})\` and let the completion reminder wake you, or use \`bash({wait:true})\` when the result is needed before anything else. The user can interrupt anytime; the wait auto-converts to an async notification. Async (background:true, requires pattern) registers a non-blocking notification and returns immediately — use when you have parallel work or want to end your turn.${noPolling}`;
}

export function createBashWatchTool(ctx: PluginContext): ToolDefinition {
  return {
    description: bashWatchDescription(
      "primary",
      bashCompanionRegistered(ctx.config, "bash_status"),
    ),
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
      timeoutMs: optionalInt(1, 1800000).describe(WATCH_TIMEOUT_PARAM_DESCRIPTION),
      deliver: z
        .literal("each_line")
        .optional()
        .describe(
          "Async-only: batch every matching complete line, fold consecutive duplicates, and report cap drops with byte offsets.",
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
      // An unknown role (the host lookup failed or was slow) follows the
      // subagent rule, as in the shell tool: an async watch would make a
      // subagent end its turn to hear the result.
      const subagentStatus = await resolveSubagentStatus(
        ctx.client,
        context.sessionID,
        context.directory,
      );
      const subagentForcedSync =
        requestedAsync && subagentStatus !== false && !bashCfg.subagent_background;
      const isSubagent = subagentStatus === true;
      const asyncMode = requestedAsync && !subagentForcedSync;
      if (args.deliver !== undefined && !asyncMode) {
        throw new Error(WATCH_EACH_LINE_NEEDS_BACKGROUND);
      }
      if (args.deliver !== undefined && args.deliver !== "each_line") {
        throw new Error("invalid_request: bash_watch deliver must be each_line");
      }

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
        if (args.deliver !== undefined) notifyParams.deliver = args.deliver;
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
        await trackV2Watch(ctx, context, taskId, registered.watch_id);
        await metadataUpdater(context)({ taskId, registered: true, watchId: registered.watch_id });
        return `Watch registered: ${registered.watch_id} on task ${taskId}\nA notification will fire when the pattern matches or the task exits.`;
      }

      const syncWaitCap = bashCfg.watch_sync_max_ms;
      const role: WatchCallerRole = isSubagent ? "worker" : "primary";
      // A worker's async request that was turned into a sync wait keeps the
      // async request's meaning, "tell me when it's done", so it waits like any
      // worker watch without a timeout: up to the worker wait limit.
      const effectiveWaitMs = subagentForcedSync
        ? bashCfg.worker_wait_max_ms
        : resolveWatchTimeoutMs(
            coerceConfiguredWatchTimeout(args.timeoutMs, role, syncWaitCap),
            role,
            syncWaitCap,
            bashCfg.worker_wait_max_ms,
          );
      const data = await waitForBashStatus(
        ctx,
        context,
        taskId,
        undefined,
        waitFor,
        effectiveWaitMs,
        role,
      );
      // The stop handler treats an unknown role as a subagent, so the stop
      // check here must too.
      const maybeSubagent = subagentStatus !== false;
      if (maybeSubagent && context.abort.aborted)
        await awaitV2ExecutionSettlement(ctx.client, context.sessionID);
      const stopped = maybeSubagent && v2SubagentStopped(ctx.client, context.sessionID);
      if (stopped && data.waited?.reason === "user_message") data.waited.reason = "aborted";
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
          waited.elapsed_ms,
          role,
        );
        await metadataUpdater(context)({
          taskId,
          status: data.status,
          waited,
          convertedToAsync: true,
        });
        return withKillDeadline(convertedText, data, role);
      }
      if (waited)
        await metadataUpdater(context)({ taskId, status: data.status, waited, effectiveWaitMs });
      return withKillDeadline(
        formatWatchResultText(taskId, data, waited, role, syncWaitCap),
        data,
        role,
      );
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
  elapsedMs: number,
  role: WatchCallerRole,
): Promise<string> {
  const interrupted = `Sync watch for task ${taskId} was interrupted because you sent a message after ${elapsedMs}ms of waiting. `;
  const reminderFallback = interruptedWatchTail(
    role,
    `The task is still running in the background. A completion reminder will be ` +
      `delivered automatically when the task exits.`,
  );
  // No pattern: the auto-reminder system already handles exit notifications
  // for background tasks, so no explicit watch registration is needed.
  if (!waitFor) {
    allowV2WatchWake(ctx.client, context.sessionID, taskId);
    return (
      interrupted +
      interruptedWatchTail(
        role,
        `The task is still running in the background. A completion reminder will be ` +
          `delivered automatically when the task exits; don't poll bash_status.`,
      )
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
        interrupted +
        `Auto-registering an async watch failed (${String(registered.message ?? "unknown error")}). ` +
        reminderFallback
      );
    }
    await trackV2Watch(ctx, context, taskId, registered.watch_id);
    return (
      interrupted +
      `The wait has been converted to an async watch (${registered.watch_id}). ` +
      interruptedWatchTail(
        role,
        `A notification will fire when the pattern matches or the task exits.`,
      )
    );
  } catch (err) {
    unmarkExplicitControl(context.sessionID, taskId);
    // Registration failed — fall back to the auto-reminder path.
    return (
      interrupted +
      `Auto-registering an async watch failed (${err instanceof Error ? err.message : String(err)}). ` +
      reminderFallback
    );
  }
}

/**
 * Every watch result also names the task's own kill deadline (or the limit
 * that killed it), kept apart from how long the watch waited.
 */
function withKillDeadline(
  text: string,
  data: Record<string, unknown>,
  role: WatchCallerRole,
): string {
  const deadline = taskKillDeadlineText(data, role);
  return deadline === "" ? text : `${text}\n${deadline}`;
}

function formatWatchResultText(
  taskId: string,
  data: Record<string, unknown>,
  waited: BashStatusWaited | undefined,
  role: WatchCallerRole,
  syncWaitCap: number,
): string {
  const status = data.status as string;
  const exit = typeof data.exit_code === "number" ? ` (exit ${data.exit_code})` : "";
  const dur =
    typeof data.duration_ms === "number" ? ` ${Math.round(data.duration_ms / 1000)}s` : "";
  let text = `Task ${taskId}: ${status}${exit}${dur}`;
  if (waited) {
    // Only a primary's watch is bounded by the configured cap.
    const waitedText = formatWatchWaited(
      waited.elapsed_ms,
      waited.limit_ms,
      role === "primary" ? syncWaitCap : undefined,
    );
    if (waited.reason === "matched") {
      const stream = waited.match_stream ? ` in ${waited.match_stream}` : "";
      text += `\n${waitedText}; matched ${JSON.stringify(waited.match ?? "")}${stream} at offset ${waited.match_offset ?? 0}.`;
    } else if (waited.reason === "timeout" && isTerminalStatus(data.status)) {
      // The task exited, but the watch ran out of time before it scanned all
      // of the captured output: neither "still running" nor "no match" is true.
      text += `\n${waitedText}; ${TERMINAL_SCAN_INCOMPLETE_TEXT}`;
    } else if (waited.reason === "timeout" && role === "worker") {
      // A watch deadline is not a failure of the command, and a delegated
      // worker that reads it as one declares a failed result mid-run. Tell it
      // the command is still running, how long it has run and what it last
      // printed, and how to wait again or stop it.
      text += `\n${waitedText}; timeout reached without match. ${workerWatchStillRunning({
        taskId,
        waitedMs: waited.elapsed_ms,
        ranMs: typeof data.duration_ms === "number" ? data.duration_ms : undefined,
        output: data.output_preview as string | undefined,
      })}`;
    } else if (waited.reason === "timeout") {
      text += `\n${waitedText}; timeout reached without match. ${watchTimeoutSteer()}`;
    } else if (waited.reason === "unavailable") {
      text += `\n${waitedText}; ${watchUnavailableSteer(role)}`;
    } else if (waited.reason === "aborted") {
      text += `\n${waitedText}; the watch was cancelled. The task keeps running.`;
    } else {
      const stat = String(data.status ?? "unknown");
      const e = typeof data.exit_code === "number" ? `, exit ${data.exit_code}` : "";
      text += `\n${waitedText}; task exited (${stat}${e}).`;
      if (waited.scan_incomplete) text += ` ${REGEX_WINDOW_SCAN_INCOMPLETE_TEXT}`;
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
  effectiveWaitMs: number | undefined,
  role: WatchCallerRole,
): Promise<BashStatusWithWait> {
  // The deadline and the reported elapsed time both come from a monotonic
  // clock, so a wall-clock step during the wait can neither end it early nor
  // inflate the time the reply says it waited. An undefined wait has no
  // deadline: it ends only on exit, a match, a new message, or an abort.
  const startedAt = watchClock.now();
  const deadline =
    effectiveWaitMs === undefined ? Number.POSITIVE_INFINITY : startedAt + effectiveWaitMs;
  const elapsedMs = () => Math.round(watchClock.now() - startedAt);
  const abortSignal = (runtime as { abort?: AbortSignal }).abort;
  // Sleep until the next poll, never past the deadline. The poll interval
  // grows with the time already waited (watchPollDelayMs).
  const pause = (pastDeadline = false) => {
    const delay = watchPollDelayMs(elapsedMs());
    return watchClock.sleep(
      pastDeadline ? delay : Math.min(delay, Math.max(0, deadline - watchClock.now())),
      abortSignal,
    );
  };
  const waited = (
    reason: BashStatusWaited["reason"],
    extra: Partial<BashStatusWaited> = {},
  ): BashStatusWaited => ({
    reason,
    elapsed_ms: elapsedMs(),
    ...(effectiveWaitMs === undefined ? {} : { limit_ms: effectiveWaitMs }),
    ...extra,
  });
  let spillCursor: OutputCursor = { output: 0, stderr: 0 };
  // One decoder per stream, so a character split across two pages decodes whole.
  const decoders = { output: new BashOutputPageDecoder(), stderr: new BashOutputPageDecoder() };
  const scanState: OutputScanState = {
    output: { text: "", baseOffset: 0, context: "" },
    stderr: { text: "", baseOffset: 0, context: "" },
  };
  // Stream byte offset of a regex match held back at a page edge, per stream
  // (see the scan below).
  const heldMatchAt: Record<OutputStream, number | undefined> = {
    output: undefined,
    stderr: undefined,
  };
  // Set once a regex scan trims older output out of its window; a later
  // no-match exit then cannot rule out a match longer than the window.
  let regexWindowDropped = false;
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
  // Start of the current run of status polls that all timed out, if any.
  let busySince: number | undefined;
  try {
    while (true) {
      if (abortSignal?.aborted) {
        return withWaited(lastData ?? unavailableSnapshot(), waited("aborted"));
      }
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
        busySince ??= watchClock.now();
        if (isSyncWatchAborted(runtime.sessionID)) {
          return withWaited(lastData ?? unavailableSnapshot(), waited("user_message"));
        }
        // A wait with no deadline still gives up on a bridge that has not
        // answered for WATCH_UNAVAILABLE_GIVE_UP_MS, instead of retrying forever.
        if (
          watchClock.now() >= deadline ||
          watchClock.now() - busySince >= WATCH_UNAVAILABLE_GIVE_UP_MS
        ) {
          return withWaited(lastData ?? unavailableSnapshot(), waited("unavailable"));
        }
        await pause();
        continue;
      }
      busySince = undefined;
      lastData = data;
      // A cancelled worker tool cannot deliver its inline result. Leave a
      // racing completion unacknowledged for the worker's next resumed turn.
      if (role === "worker" && abortSignal?.aborted) {
        return withWaited(data, waited("aborted"));
      }
      const terminal = isTerminalStatus(data.status);
      if (waitFor) {
        const scan = await readNewTaskOutput(data, spillCursor, decoders);
        spillCursor = scan.nextCursor;
        // Independent buffers prevent a stdout suffix and stderr prefix from
        // becoming a fabricated match. When both streams match in one poll,
        // stdout is checked first and wins.
        for (const stream of OUTPUT_STREAMS) {
          const chunk = scan.chunks.find((candidate) => candidate.stream === stream);
          const more = streamHasMore(data, stream);
          // A held-back match is checked again once its stream is known to end there.
          const held = heldMatchAt[stream];
          if (!chunk && !(held !== undefined && !more)) continue;
          heldMatchAt[stream] = undefined;
          const state = scanState[stream];
          if (chunk) {
            if (state.text.length === 0) state.baseOffset = chunk.baseOffset;
            state.text += chunk.text;
            if (waitFor.kind === "regex") {
              const trimmed = trimWaitScanBuffer(
                state.text,
                state.baseOffset,
                waitFor,
                held,
                state.context,
              );
              if (trimmed.baseOffset !== state.baseOffset) regexWindowDropped = true;
              state.text = trimmed.text;
              state.baseOffset = trimmed.baseOffset;
              state.context = trimmed.context;
            }
          }
          const match = await findWaitMatch(ctx, runtime, state.text, waitFor, state.context);
          if (match && more && regexMatchReachesBufferEnd(waitFor, match, state.text)) {
            // A page ends where the read stopped, not where the output ends.
            // A regex match reaching the end of the buffer may depend on what
            // follows ($, \z, \b, a greedy repeat), so it waits for the next page.
            heldMatchAt[stream] = state.baseOffset + match.byteOffset;
          } else if (match) {
            if (terminal) {
              sawTerminal = true;
              consumeBgCompletion(runtime.sessionID, taskId);
              await markBgCompletionDelivered(
                { ctx, directory: projectRootFor(runtime), sessionID: runtime.sessionID },
                taskId,
              );
            }
            const matchStream: "stdout" | "stderr" | undefined =
              data.mode === "pty" ? undefined : stream === "output" ? "stdout" : "stderr";
            return withWaited(
              data,
              waited("matched", {
                match: match.text,
                match_offset: state.baseOffset + match.byteOffset,
                match_stream: matchStream,
              }),
            );
          }
          if (waitFor.kind === "substring") {
            const trimmed = trimWaitScanBuffer(state.text, state.baseOffset, waitFor);
            state.text = trimmed.text;
            state.baseOffset = trimmed.baseOffset;
          }
        }
      }
      // An exited task can still have captured output past this page. Scan
      // it all, within the watch's deadline, before reporting the exit.
      const outputPending =
        !!waitFor && (data.output_has_more === true || data.stderr_has_more === true);
      if (terminal && !outputPending) {
        sawTerminal = true;
        consumeBgCompletion(runtime.sessionID, taskId);
        await markBgCompletionDelivered(
          { ctx, directory: projectRootFor(runtime), sessionID: runtime.sessionID },
          taskId,
        );
        return withWaited(
          data,
          waited("exited", regexWindowDropped ? { scan_incomplete: true } : {}),
        );
      }
      // User-message abort: if the user sent a message while we were
      // blocking, convert this sync wait to an async watch so the agent's
      // turn ends promptly. The match/exit checks above win over abort —
      // if the task already matched or exited in this iteration, we return
      // that result instead of aborting.
      if (isSyncWatchAborted(runtime.sessionID)) {
        return withWaited(data, waited("user_message"));
      }
      const waitPastDeadline =
        role === "worker" &&
        watchClock.now() >= deadline &&
        taskKillDeadlineWithinHandoffMargin(data);
      if (watchClock.now() >= deadline && !waitPastDeadline) {
        return withWaited(data, waited("timeout"));
      }
      if (outputPending) continue;
      await pause(waitPastDeadline);
    }
  } finally {
    if (!sawTerminal) unmarkTaskWaiting(runtime.sessionID, taskId);
  }
}

async function readNewTaskOutput(
  data: Record<string, unknown>,
  cursor: OutputCursor,
  decoders: Record<OutputStream, BashOutputPageDecoder>,
): Promise<{ chunks: OutputScanChunk[]; nextCursor: OutputCursor }> {
  const stdoutBytes =
    typeof data.output_chunk_base64 === "string"
      ? Buffer.from(data.output_chunk_base64, "base64")
      : Buffer.alloc(0);
  const stderrBytes =
    typeof data.stderr_chunk_base64 === "string"
      ? Buffer.from(data.stderr_chunk_base64, "base64")
      : Buffer.alloc(0);
  // A stream really ends only when its task has exited and no captured
  // bytes remain; only then may a held-back partial character be flushed.
  const terminal = isTerminalStatus(data.status);
  const stdout = decoders.output.decode(
    stdoutBytes,
    cursor.output,
    terminal && data.output_has_more !== true,
  );
  const stderr = decoders.stderr.decode(
    stderrBytes,
    cursor.stderr,
    terminal && data.stderr_has_more !== true,
  );
  const chunks: OutputScanChunk[] = [];
  if (stdout.text.length > 0) {
    chunks.push({ stream: "output", text: stdout.text, baseOffset: stdout.baseOffset });
  }
  if (stderr.text.length > 0) {
    chunks.push({ stream: "stderr", text: stderr.text, baseOffset: stderr.baseOffset });
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

const OUTPUT_STREAMS: readonly OutputStream[] = ["output", "stderr"];

/** Whether bash_status reported captured bytes past this page for `stream`. */
function streamHasMore(data: Record<string, unknown>, stream: OutputStream): boolean {
  return (stream === "output" ? data.output_has_more : data.stderr_has_more) === true;
}

/** A regex match that ends exactly at the end of the scanned text. A literal
 * is complete once its bytes are in, so it never qualifies. */
function regexMatchReachesBufferEnd(
  pattern: BashWaitPattern,
  match: WaitMatch,
  text: string,
): boolean {
  return (
    pattern.kind === "regex" &&
    match.byteOffset + Buffer.byteLength(match.text, "utf8") === Buffer.byteLength(text, "utf8")
  );
}

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
  context = "",
): Promise<WaitMatch | undefined> {
  if (pattern.kind === "substring") {
    const index = text.indexOf(pattern.value);
    return index >= 0
      ? { text: pattern.value, byteOffset: Buffer.byteLength(text.slice(0, index), "utf8") }
      : undefined;
  }
  return await matchRegexWithBridge(ctx, runtime, pattern.source, text, context);
}

async function matchRegexWithBridge(
  ctx: PluginContext,
  runtime: ToolContext,
  pattern: string,
  text: string,
  context = "",
): Promise<WaitMatch | undefined> {
  const start = Buffer.byteLength(context, "utf8");
  const result = await callBashBridge(
    ctx,
    runtime,
    "bash_regex_match",
    start === 0 ? { pattern, text } : { pattern, text: context + text, start },
  );
  if (result.success === false) {
    const code = String(result.code ?? "invalid_request");
    const message = String(result.message ?? "bash_regex_match failed");
    if (code === "invalid_regex") throw new Error(`invalid_request: invalid_regex: ${message}`);
    throw new Error(`${code}: ${message}`);
  }
  if (result.matched !== true) return undefined;
  return contextRelativeMatch(result, start);
}

/**
 * A bash_regex_match reply as a match in the text after `start` bytes of
 * context. An engine that echoes `start` searched from there. An older engine
 * ignores `start` and searches from the beginning, so a match it reports
 * inside the context may rest on a false start of input; it is dropped,
 * missing a match rather than inventing one.
 */
function contextRelativeMatch(
  result: Record<string, unknown>,
  start: number,
): WaitMatch | undefined {
  const offset = coerceMatchOffset(result.match_offset);
  if (offset < start) return undefined;
  return {
    text: typeof result.match_text === "string" ? result.match_text : "",
    byteOffset: offset - start,
  };
}

function coerceMatchOffset(value: unknown): number {
  const offset = typeof value === "number" ? value : Number(value ?? 0);
  return Number.isFinite(offset) && offset >= 0 ? offset : 0;
}

/**
 * Trims the scan buffer to what a later match still needs. `heldAt`, the
 * stream byte offset of a regex match held back at a page edge, keeps that
 * match whole until the next page settles it, as long as the kept text stays
 * within REGEX_WAIT_HELD_MATCH_CAP_BYTES. An open-ended match (for example
 * `BEGIN[\s\S]*`) would otherwise pin the buffer start and grow it by a page
 * on every poll.
 */
function trimWaitScanBuffer(
  text: string,
  baseOffset: number,
  pattern: BashWaitPattern,
  heldAt?: number,
  context = "",
): { text: string; baseOffset: number; context: string } {
  let keepFrom =
    pattern.kind === "substring"
      ? substringKeepStart(text, pattern.value)
      : regexKeepStart(text, REGEX_WAIT_SCAN_WINDOW_BYTES);
  if (
    heldAt !== undefined &&
    heldAt >= baseOffset &&
    Buffer.byteLength(text, "utf8") - (heldAt - baseOffset) <= REGEX_WAIT_HELD_MATCH_CAP_BYTES
  ) {
    keepFrom = Math.min(keepFrom, charIndexAtByteOffset(text, heldAt - baseOffset));
  }
  if (keepFrom <= 0) return { text, baseOffset, context };

  return {
    text: text.slice(keepFrom),
    baseOffset: baseOffset + Buffer.byteLength(text.slice(0, keepFrom), "utf8"),
    context: characterBefore(text, keepFrom),
  };
}

/**
 * The whole character (one or two UTF-16 units) just before `index`. A lone
 * surrogate is never returned, because it cannot be sent as JSON text.
 */
function characterBefore(text: string, index: number): string {
  if (index <= 0) return "";
  const last = text.charCodeAt(index - 1);
  if (isLowSurrogate(last)) {
    return index >= 2 && isHighSurrogate(text.charCodeAt(index - 2))
      ? text.slice(index - 2, index)
      : "";
  }
  return isHighSurrogate(last) ? "" : text.slice(index - 1, index);
}

/** The string index of the character that starts `byteOffset` bytes into `text`. */
function charIndexAtByteOffset(text: string, byteOffset: number): number {
  return Buffer.from(text, "utf8").subarray(0, byteOffset).toString("utf8").length;
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
  // Never start the window inside a surrogate pair: a lone surrogate cannot
  // be sent as JSON text. Moving forward past the pair keeps the byte bound.
  return isLowSurrogate(text.charCodeAt(low)) && isHighSurrogate(text.charCodeAt(low - 1))
    ? low + 1
    : low;
}

function isHighSurrogate(code: number): boolean {
  return code >= 0xd800 && code <= 0xdbff;
}

function isLowSurrogate(code: number): boolean {
  return code >= 0xdc00 && code <= 0xdfff;
}

export function __trimWaitScanBufferForTests(
  text: string,
  baseOffset: number,
  pattern: BashWaitPattern,
): { text: string; baseOffset: number; context: string } {
  return trimWaitScanBuffer(text, baseOffset, pattern);
}

function withWaited(data: Record<string, unknown>, waited: BashStatusWaited): BashStatusWithWait {
  return { ...data, waited };
}
