import { createHash, randomUUID } from "node:crypto";
import { sessionLog, sessionWarn } from "./logger.js";
import { resolvePromptContext } from "./shared/last-assistant-model.js";
import { getLiveServerClient, useLiveServerWake } from "./shared/live-server-client.js";
import type { PluginContext } from "./types.js";

/**
 * Short SHA-256 of the reminder body for delivery-trace correlation. The full
 * body is never logged (it can contain large output previews); 16 hex chars is
 * enough to uniquely identify a unique reminder within a session.
 */
function hashReminder(text: string): string {
  return createHash("sha256").update(text).digest("hex").slice(0, 16);
}

export interface BgCompletion {
  task_id: string;
  status: string;
  exit_code: number | null;
  command: string;
  duration_ms?: number;
  runtime_ms?: number;
  runtime?: number;
  /** Tail of stdout+stderr captured at completion (≤300 bytes from Rust). */
  output_preview?: string;
  /** True when the captured tail is shorter than the actual output. */
  output_truncated?: boolean;
  // Token counts arrive in v0.27 but commit 7 leaves them unused.
  // Commit 13 will write them to storage via aft_db_record_compression.
  original_tokens?: number;
  compressed_tokens?: number;
  tokens_skipped?: boolean;
}

export interface BgLongRunningReminder {
  task_id: string;
  session_id: string;
  command: string;
  elapsed_ms: number;
}

type SessionBgState = {
  outstandingTaskIds: Set<string>;
  pendingCompletions: BgCompletion[];
  pendingLongRunning: BgLongRunningReminder[];
  debounceTimer: NodeJS.Timeout | null;
  firstCompletionAt: number | null;
  scheduledFireAt: number | null;
  scheduledCompletionCount: number;
  retryDelayMs: number | null;
  wakeRetryAttempts: number;
  wakeHardStopped: boolean;
  forcedDrainCompleted: boolean;
  unknownCompletions: Array<{ completion: BgCompletion; receivedAt: number }>;
  lastSeenAt: number;
};

export const sessionBgStates: Map<string, SessionBgState> = new Map();

// Lazily evict idle, task-free sessions after 1 hour; no timer is used so the plugin doesn't keep the event loop alive.
export const SESSION_BG_STATE_IDLE_TTL_MS = 60 * 60 * 1000;
const DEBOUNCE_STEP_MS = 200;
const DEBOUNCE_CAP_MS = 1000;
const MAX_WAKE_SEND_ATTEMPTS = 5;
const UNKNOWN_COMPLETION_TTL_MS = 5000;
const UNKNOWN_COMPLETION_CAP = 32;
const DEFAULT_SESSION_ID = "__default__";
const LOG_PREFIX = "[aft-plugin] bg-notifications:";

interface DrainContext {
  ctx: PluginContext;
  directory: string;
  sessionID: string;
  /**
   * Plugin-provided OpenCode SDK client (`input.client`). The wake path
   * uses this as a fallback when `useLiveServerWake()` is false — i.e.
   * the live HTTP listener was unreachable when probed at plugin init,
   * so `getLiveServerClient(...)` cannot be built. Falling back here
   * accepts the upstream `promptAsync` runner-split bug
   * (anomalyco/opencode#28202; duplicate "stop" messages) in exchange
   * for wakes still arriving at all in plain-TUI sessions.
   *
   * Typed `unknown` because the real `@opencode-ai/sdk` `OpencodeClient`
   * has a narrower, generated `promptAsync` signature than the loose
   * structural `OpenCodeClient` shape used by the live-server factory
   * and test stubs. The wake closure asserts to `OpenCodeClient` after
   * deciding which transport to use.
   */
  client?: unknown;
  /**
   * Live OpenCode HTTP listener URL (from `input.serverUrl`). When the
   * listener was reachable at startup, the wake path builds a separate
   * `createOpencodeClient` from this URL so requests hit the same Effect
   * memoMap as the live UI — works around the runner-split bug
   * (anomalyco/opencode#28202). When the listener was unreachable, the
   * wake path falls back to `client` above; this URL is unused.
   */
  serverUrl?: string;
}

interface OpenCodeClient {
  session?: {
    promptAsync?: (input: unknown) => Promise<unknown> | unknown;
    messages?: (input: { path: { id: string } }) => Promise<{ data?: unknown[] }>;
  };
}

export function trackBgTask(sessionID: string | undefined, taskId: string): void {
  const state = stateFor(sessionID);
  pruneUnknownCompletions(state, Date.now());
  const buffered = state.unknownCompletions.filter((entry) => entry.completion.task_id === taskId);
  state.unknownCompletions = state.unknownCompletions.filter(
    (entry) => entry.completion.task_id !== taskId,
  );
  if (buffered.length > 0) {
    for (const entry of buffered) {
      if (!state.pendingCompletions.some((pending) => pending.task_id === taskId)) {
        state.pendingCompletions.push(entry.completion);
      }
    }
    return;
  }
  state.outstandingTaskIds.add(taskId);
}

export function ingestBgCompletions(
  sessionID: string | undefined,
  completions: unknown,
): BgCompletion[] {
  if (!Array.isArray(completions) || completions.length === 0) return [];
  const state = stateFor(sessionID);
  const accepted: BgCompletion[] = [];
  for (const completion of completions) {
    if (!isBgCompletion(completion)) continue;
    if (!state.outstandingTaskIds.has(completion.task_id)) {
      bufferUnknownCompletion(state, completion);
      continue;
    }
    state.outstandingTaskIds.delete(completion.task_id);
    if (
      !state.pendingCompletions.some((pending) => pending.task_id === completion.task_id) &&
      !accepted.some((pending) => pending.task_id === completion.task_id)
    ) {
      accepted.push(completion);
    }
  }
  state.pendingCompletions.push(...accepted);
  return accepted;
}

export async function handlePushedBgCompletion(
  drainContext: DrainContext & { client: unknown },
  completion: unknown,
): Promise<void> {
  ingestBgCompletions(drainContext.sessionID, [completion]);
  await triggerWakeIfPending(drainContext, true);
}

export async function handlePushedBgLongRunning(
  drainContext: DrainContext & { client: unknown },
  reminder: BgLongRunningReminder,
): Promise<void> {
  stateFor(drainContext.sessionID).pendingLongRunning.push(reminder);
  await triggerWakeIfPending(drainContext, true);
}

export async function appendInTurnBgCompletions(
  drainContext: DrainContext,
  output: { output?: string } | undefined,
): Promise<void> {
  if (!output) return;
  const state = stateFor(drainContext.sessionID);
  if (
    state.outstandingTaskIds.size === 0 &&
    state.pendingCompletions.length === 0 &&
    state.pendingLongRunning.length === 0
  ) {
    await drainCompletions(drainContext);
    if (
      state.outstandingTaskIds.size === 0 &&
      state.pendingCompletions.length === 0 &&
      state.pendingLongRunning.length === 0
    ) {
      return;
    }
  }

  if (state.outstandingTaskIds.size > 0 || !state.forcedDrainCompleted) {
    await drainCompletions(drainContext);
  }
  if (state.pendingCompletions.length === 0 && state.pendingLongRunning.length === 0) return;

  const deliveredCompletions = [...state.pendingCompletions];
  const reminder = formatCombinedSystemReminder(state.pendingCompletions, state.pendingLongRunning);
  output.output = appendReminder(output.output ?? "", reminder);
  // Trace #7 of 7: reminder went out as part of an existing tool result
  // instead of through promptAsync. NO wake_prompt_async_start event
  // accompanies this branch — that's the diagnostic signal that the
  // reminder reached the model via tool-result piggyback.
  sessionLog(drainContext.sessionID, `${LOG_PREFIX} in-turn append`, {
    event: "bash_completion_in_turn_append",
    task_ids: deliveredCompletions.map((c) => c.task_id),
    long_running_task_ids: state.pendingLongRunning.map((r) => r.task_id),
    reminder_sha256: hashReminder(reminder),
    reminder_chars: reminder.length,
  });
  state.pendingCompletions = [];
  state.pendingLongRunning = [];
  state.wakeRetryAttempts = 0;
  state.wakeHardStopped = false;
  await ackCompletions(drainContext, deliveredCompletions);
  // Cancel any pending debounced wake — its captured pendingCompletions /
  // pendingLongRunning are now drained, and firing the timer anyway would
  // build an empty-body system-reminder ("[BACKGROUND BASH STILL RUNNING]"
  // with no bullets) since the timer reads `state.pendingLongRunning`
  // again at fire time.
  if (state.debounceTimer) {
    clearTimeout(state.debounceTimer);
    state.debounceTimer = null;
    state.firstCompletionAt = null;
    state.scheduledFireAt = null;
    state.scheduledCompletionCount = 0;
  }
}

export async function handleIdleBgCompletions(
  drainContext: DrainContext & { client: unknown },
): Promise<void> {
  await triggerWakeIfPending(drainContext, false);
}

async function triggerWakeIfPending(
  drainContext: DrainContext & { client: unknown },
  skipDrain: boolean,
): Promise<void> {
  // Note: previously bailed on `isActive()` (bridge.hasPendingRequests())
  // to defer wakes until the bridge was idle. That was wrong:
  // bridge.hasPendingRequests() returns true for the TUI status RPC poll
  // and any other non-agent traffic. When a bash_completed push arrived
  // during such a window, we'd skip scheduling the wake — and the only
  // recovery paths (session.idle and appendInTurnBgCompletions) can
  // legitimately not fire in time, leaving the agent waiting forever.
  // The downstream debounce (200-1000ms), appendInTurnBgCompletions's
  // timer cancellation, and the send-failure retry-with-backoff already
  // handle the original concern (avoiding mid-turn wake races) correctly.
  const state = stateFor(drainContext.sessionID);

  if (!skipDrain && (state.outstandingTaskIds.size > 0 || !state.forcedDrainCompleted)) {
    await drainCompletions(drainContext);
  }
  if (state.pendingCompletions.length === 0 && state.pendingLongRunning.length === 0) return;

  scheduleWake(
    state,
    async (reminder, deliveredCompletions) => {
      // Wake transport selection (per-process decision, set once at plugin
      // init via `setLiveServerWakeAvailable()`):
      //
      //   • `useLiveServerWake() === true`  — live HTTP listener was
      //     reachable when probed at startup. Build a separate
      //     `createOpencodeClient` pointed at `input.serverUrl` so requests
      //     hit the same Effect memoMap as the live UI. This works around
      //     the promptAsync runner-split bug (anomalyco/opencode#28202).
      //
      //   • `useLiveServerWake() === false` — listener was unreachable
      //     (typically plain TUI started without `opencode --port 0`).
      //     Fall back to `drainContext.client.session.promptAsync`. This
      //     accepts the upstream duplicate-runner bug in exchange for
      //     wakes still arriving at all instead of throwing each turn.
      //
      // The choice is logged via `wake_client_path` so post-mortem can
      // tell which transport delivered each wake.
      let client: OpenCodeClient;
      let clientPath: "live-server" | "in-process-fallback";
      if (useLiveServerWake() && drainContext.serverUrl) {
        client = getLiveServerClient(
          drainContext.serverUrl,
          drainContext.directory,
        ) as OpenCodeClient;
        clientPath = "live-server";
      } else {
        if (!drainContext.client) {
          sessionWarn(drainContext.sessionID, `${LOG_PREFIX} wake client unavailable`, {
            event: "bash_completion_wake_client_unavailable",
            task_ids: deliveredCompletions.map((c) => c.task_id),
            directory: drainContext.directory,
            attempt: state.wakeRetryAttempts + 1,
          });
          throw new Error(
            "no wake transport available: live-server unreachable and input.client absent",
          );
        }
        // Cast the unknown `input.client` (real SDK shape with a generated
        // narrower promptAsync signature) to the loose structural shape
        // the wake closure uses. The runtime check on
        // `client.session?.promptAsync` below confirms shape before use.
        client = drainContext.client as OpenCodeClient;
        clientPath = "in-process-fallback";
      }
      if (typeof client.session?.promptAsync !== "function") {
        throw new Error(`wake client.session.promptAsync is unavailable (path=${clientPath})`);
      }
      // Pass the previous turn's prompt context (agent + model + variant)
      // explicitly. OpenCode's `createUserMessage` resolves variant
      // relative to the chosen agent's model — passing model alone makes
      // OpenCode pick the default agent and its model match check fails,
      // bypassing our variant. This call uses noReply: false so it DOES
      // trigger an assistant turn — preserving cache here matters.
      // Mirrors the resolution `opencode-xtra` uses for its
      // background-agent notifications. See shared/last-assistant-model.ts.
      const promptContext = await resolvePromptContext(client, drainContext.sessionID);
      const body: Record<string, unknown> = {
        noReply: false,
        parts: [{ type: "text", text: reminder }],
      };
      if (promptContext?.agent) body.agent = promptContext.agent;
      if (promptContext?.model) {
        body.model = {
          providerID: promptContext.model.providerID,
          modelID: promptContext.model.modelID,
        };
      }
      if (promptContext?.variant) body.variant = promptContext.variant;

      // Trace #3 of 7: about to call promptAsync. The deliveryID uniquely
      // identifies this single promptAsync invocation across the rest of
      // the trace chain (#3 start → #4 ok / #5 error → #6 ack_ok). One
      // deliveryID = one HTTP POST to OpenCode's session prompt endpoint.
      // When the DB shows multiple assistant children but logs show one
      // start event with this deliveryID, the duplication is downstream
      // of AFT.
      const deliveryID = `aftdel_${randomUUID()}`;
      const taskIDs = deliveredCompletions.map((c) => c.task_id);
      const wakeMeta = {
        delivery_id: deliveryID,
        attempt: state.wakeRetryAttempts + 1,
        task_ids: taskIDs,
        directory: drainContext.directory,
        reminder_sha256: hashReminder(reminder),
        reminder_chars: reminder.length,
        // `live-server` = wake POSTed through `createOpencodeClient` aimed
        // at `input.serverUrl` (anomalyco/opencode#28202 workaround, no
        // duplicate runs). `in-process-fallback` = wake POSTed through
        // `input.client.session.promptAsync` because the live listener
        // wasn't reachable at startup; this accepts the upstream bug so
        // wakes still arrive at all instead of throwing each turn.
        wake_client_path: clientPath,
        prompt_context: promptContext
          ? {
              agent: promptContext.agent,
              model: promptContext.model
                ? {
                    providerID: promptContext.model.providerID,
                    modelID: promptContext.model.modelID,
                  }
                : null,
              variant: promptContext.variant ?? null,
            }
          : null,
      };
      sessionLog(drainContext.sessionID, `${LOG_PREFIX} wake promptAsync start`, {
        event: "bash_completion_wake_prompt_async_start",
        ...wakeMeta,
      });
      try {
        await client.session.promptAsync({
          path: { id: drainContext.sessionID },
          body,
        });
      } catch (err) {
        // Trace #5 of 7: promptAsync rejected. Counted toward MAX_WAKE_SEND_ATTEMPTS
        // by the catch in scheduleWake. Re-throw so the retry path runs.
        sessionWarn(drainContext.sessionID, `${LOG_PREFIX} wake promptAsync error`, {
          event: "bash_completion_wake_prompt_async_error",
          delivery_id: deliveryID,
          attempt: state.wakeRetryAttempts + 1,
          task_ids: taskIDs,
          error: err instanceof Error ? err.message : String(err),
        });
        throw err;
      }
      // Trace #4 of 7: promptAsync resolved. OpenCode has accepted the
      // synthetic user message and will run the agent turn. A subsequent
      // assistant child with finish="stop" should appear in OpenCode's
      // DB for this parent user message; if MORE than one appears for
      // the same parent + reminder_sha256, the duplication is in the
      // OpenCode runner, not in AFT (only one promptAsync call exists
      // with this deliveryID in the log).
      sessionLog(drainContext.sessionID, `${LOG_PREFIX} wake promptAsync ok`, {
        event: "bash_completion_wake_prompt_async_ok",
        delivery_id: deliveryID,
        attempt: state.wakeRetryAttempts + 1,
        task_ids: taskIDs,
      });
      await ackCompletions(drainContext, deliveredCompletions, deliveryID);
    },
    (err, hardStopped) => {
      sessionWarn(
        drainContext.sessionID,
        hardStopped
          ? `${LOG_PREFIX} wake send failed ${MAX_WAKE_SEND_ATTEMPTS} times; stopping retries: ${err instanceof Error ? err.message : String(err)}`
          : `${LOG_PREFIX} wake send failed: ${err instanceof Error ? err.message : String(err)}`,
      );
    },
    drainContext.sessionID,
  );
}

export function formatSystemReminder(completions: readonly BgCompletion[]): string {
  const bullets = completions.map((completion) => formatCompletion(completion)).join("\n");
  // Only point at bash_status when at least one completion is truncated;
  // for fully-captured short outputs the agent already has the full result.
  const anyTruncated = completions.some((c) => c.output_truncated === true);
  const tail = anyTruncated
    ? `\n\nFor truncated tasks, use bash_status({ taskId: "..." }) to retrieve full output.`
    : "";
  return `<system-reminder>\n[BACKGROUND BASH COMPLETED]\n${bullets}${tail}\n</system-reminder>`;
}

export function formatLongRunningReminder(reminders: readonly BgLongRunningReminder[]): string {
  const bullets = reminders
    .map(
      (reminder) =>
        `- ${reminder.task_id} still running after ${formatDurationMs(reminder.elapsed_ms)}: ${shorten(reminder.command, 120)}`,
    )
    .join("\n");
  return `<system-reminder>\n[BACKGROUND BASH STILL RUNNING]\n${bullets}\nUse bash_status({ taskId: "..." }) to inspect output or bash_kill({ taskId: "..." }) to terminate.\n</system-reminder>`;
}

function formatCombinedSystemReminder(
  completions: readonly BgCompletion[],
  longRunning: readonly BgLongRunningReminder[],
): string {
  if (completions.length === 0) return formatLongRunningReminder(longRunning);
  if (longRunning.length === 0) return formatSystemReminder(completions);
  return `${formatSystemReminder(completions)}\n${formatLongRunningReminder(longRunning)}`;
}

export function extractSessionID(value: unknown): string | undefined {
  if (!value || typeof value !== "object") return undefined;
  const record = value as Record<string, unknown>;
  for (const key of ["sessionID", "sessionId", "id"]) {
    if (typeof record[key] === "string") return record[key];
  }
  const info = record.info;
  if (info && typeof info === "object") {
    const nested = info as Record<string, unknown>;
    for (const key of ["sessionID", "sessionId", "id"]) {
      if (typeof nested[key] === "string") return nested[key];
    }
  }
  return undefined;
}

export function __resetBgNotificationStateForTests(): void {
  for (const state of sessionBgStates.values()) {
    if (state.debounceTimer) clearTimeout(state.debounceTimer);
  }
  sessionBgStates.clear();
}

async function drainCompletions({ ctx, directory, sessionID }: DrainContext): Promise<void> {
  const state = stateFor(sessionID);
  try {
    const bridge = ctx.pool.getActiveBridgeForRoot(directory) ?? ctx.pool.getBridge(directory);
    const response = await bridge.send("bash_drain_completions", { session_id: sessionID });
    if (response.success === false) {
      sessionWarn(
        sessionID,
        `${LOG_PREFIX} drain failed: ${String(response.message ?? "unknown error")}`,
      );
      return;
    }
    state.forcedDrainCompleted = true;
    ingestDrainedBgCompletions(sessionID, response.bg_completions);
  } catch (err) {
    sessionWarn(
      sessionID,
      `${LOG_PREFIX} drain failed: ${err instanceof Error ? err.message : String(err)}`,
    );
  }
}

async function ackCompletions(
  { ctx, directory, sessionID }: DrainContext,
  completions: readonly BgCompletion[],
  deliveryID?: string,
): Promise<void> {
  const taskIds = [...new Set(completions.map((completion) => completion.task_id))];
  if (taskIds.length === 0) return;
  try {
    const bridge = ctx.pool.getActiveBridgeForRoot(directory) ?? ctx.pool.getBridge(directory);
    const response = await bridge.send("bash_ack_completions", {
      session_id: sessionID,
      task_ids: taskIds,
    });
    if (response.success === false) {
      sessionWarn(
        sessionID,
        `${LOG_PREFIX} ack failed: ${String(response.message ?? "unknown error")}`,
      );
      return;
    }
    // Trace #6 of 7: bash_ack_completions succeeded on the Rust side.
    // Closes the wake chain: scheduled → fire → start → ok → ack_ok.
    // Note: ack also runs from appendInTurnBgCompletions without a
    // deliveryID — that path uses trace #7 (in_turn_append) instead, so
    // ack_ok carries delivery_id only when present.
    sessionLog(sessionID, `${LOG_PREFIX} ack ok`, {
      event: "bash_completion_ack_ok",
      delivery_id: deliveryID ?? null,
      task_ids: taskIds,
    });
  } catch (err) {
    sessionWarn(
      sessionID,
      `${LOG_PREFIX} ack failed: ${err instanceof Error ? err.message : String(err)}`,
    );
  }
}

function scheduleWake(
  state: SessionBgState,
  sendWake: (reminder: string, completions: readonly BgCompletion[]) => Promise<void>,
  onSendFailure: (err: unknown, hardStopped: boolean) => void,
  sessionID?: string,
): void {
  if (state.wakeHardStopped) return;
  // Race model: JS state changes are synchronous; awaits only happen before scheduling
  // drains and during final prompt delivery. Multiple hook invocations can interleave
  // only at those awaits, so we gate timer extension on the pending completion count.
  const now = Date.now();
  const pendingCount = state.pendingCompletions.length + state.pendingLongRunning.length;
  if (state.debounceTimer && pendingCount <= state.scheduledCompletionCount) {
    return;
  }
  if (state.firstCompletionAt === null) {
    state.firstCompletionAt = now;
    state.scheduledFireAt = now + DEBOUNCE_STEP_MS;
  } else {
    const previousFireAt = state.scheduledFireAt ?? now;
    state.scheduledFireAt = Math.min(
      previousFireAt + DEBOUNCE_STEP_MS,
      state.firstCompletionAt + DEBOUNCE_CAP_MS,
    );
  }
  state.scheduledCompletionCount = pendingCount;

  if (state.debounceTimer) clearTimeout(state.debounceTimer);
  const delay = state.retryDelayMs ?? Math.max(0, (state.scheduledFireAt ?? now) - now);

  // Trace #1 of 7 for the wake-delivery chain. Pairs with bash_completion_wake_fire.
  // When the OpenCode DB later shows N assistant children for one parent
  // user message, the matching count of wake_scheduled / wake_fire /
  // wake_prompt_async_start events for the same task_ids tells us whether
  // AFT submitted the prompt once or N times. See
  // .alfonso/incident-reports/2026-05-21-bash-reminder-duplicate-runs.md.
  sessionLog(sessionID, `${LOG_PREFIX} wake scheduled`, {
    event: "bash_completion_wake_scheduled",
    delay_ms: delay,
    pending_completions: state.pendingCompletions.length,
    pending_long_running: state.pendingLongRunning.length,
    retry_attempt: state.wakeRetryAttempts,
  });

  state.debounceTimer = setTimeout(() => {
    const pending = state.pendingCompletions;
    const pendingLongRunning = state.pendingLongRunning;
    state.debounceTimer = null;
    state.firstCompletionAt = null;
    state.scheduledFireAt = null;
    state.scheduledCompletionCount = 0;
    // Defensive: if another path (e.g. appendInTurnBgCompletions) drained the
    // pending arrays between schedule and fire and didn't cancel us, just
    // skip — don't ship an empty "[BACKGROUND BASH STILL RUNNING]" shell.
    if (pending.length === 0 && pendingLongRunning.length === 0) return;
    const reminder = formatCombinedSystemReminder(pending, pendingLongRunning);

    // Trace #2 of 7: timer actually fired and we captured a non-empty
    // pending set. The matching wake_prompt_async_start MUST follow within
    // ~milliseconds — its absence means sendWake threw synchronously
    // before reaching client.session.promptAsync.
    sessionLog(sessionID, `${LOG_PREFIX} wake fire`, {
      event: "bash_completion_wake_fire",
      task_ids: pending.map((c) => c.task_id),
      long_running_task_ids: pendingLongRunning.map((r) => r.task_id),
      reminder_sha256: hashReminder(reminder),
      reminder_chars: reminder.length,
      retry_attempt: state.wakeRetryAttempts,
    });

    state.pendingCompletions = [];
    state.pendingLongRunning = [];
    void sendWake(reminder, pending)
      .then(() => {
        state.retryDelayMs = null;
        state.wakeRetryAttempts = 0;
        state.wakeHardStopped = false;
      })
      .catch((err) => {
        state.pendingCompletions = [...pending, ...state.pendingCompletions];
        state.pendingLongRunning = [...pendingLongRunning, ...state.pendingLongRunning];
        state.wakeRetryAttempts += 1;
        if (state.wakeRetryAttempts >= MAX_WAKE_SEND_ATTEMPTS) {
          state.retryDelayMs = null;
          state.wakeHardStopped = true;
          onSendFailure(err, true);
          return;
        }
        state.retryDelayMs = Math.min((delay || DEBOUNCE_STEP_MS) * 2, DEBOUNCE_CAP_MS);
        onSendFailure(err, false);
        scheduleWake(state, sendWake, onSendFailure, sessionID);
      });
  }, delay);
  state.debounceTimer.unref?.();
}

function stateFor(sessionID: string | undefined): SessionBgState {
  const now = Date.now();
  cleanupIdleSessionStates(now);
  const key = sessionID || DEFAULT_SESSION_ID;
  let state = sessionBgStates.get(key);
  if (!state) {
    state = {
      outstandingTaskIds: new Set(),
      pendingCompletions: [],
      pendingLongRunning: [],
      debounceTimer: null,
      firstCompletionAt: null,
      scheduledFireAt: null,
      scheduledCompletionCount: 0,
      retryDelayMs: null,
      wakeRetryAttempts: 0,
      wakeHardStopped: false,
      forcedDrainCompleted: false,
      unknownCompletions: [],
      lastSeenAt: now,
    };
    sessionBgStates.set(key, state);
  } else {
    state.lastSeenAt = now;
  }
  return state;
}

function ingestDrainedBgCompletions(
  sessionID: string | undefined,
  completions: unknown,
): BgCompletion[] {
  if (!Array.isArray(completions) || completions.length === 0) return [];
  const state = stateFor(sessionID);
  const accepted: BgCompletion[] = [];
  for (const completion of completions) {
    if (!isBgCompletion(completion)) continue;
    state.outstandingTaskIds.delete(completion.task_id);
    if (
      !state.pendingCompletions.some((pending) => pending.task_id === completion.task_id) &&
      !accepted.some((pending) => pending.task_id === completion.task_id)
    ) {
      accepted.push(completion);
    }
  }
  state.pendingCompletions.push(...accepted);
  return accepted;
}

function cleanupIdleSessionStates(now: number): void {
  const cutoff = now - SESSION_BG_STATE_IDLE_TTL_MS;
  for (const [sessionID, state] of sessionBgStates) {
    if (state.outstandingTaskIds.size > 0) continue;
    if (state.lastSeenAt >= cutoff) continue;
    if (state.debounceTimer) clearTimeout(state.debounceTimer);
    sessionBgStates.delete(sessionID);
  }
}

function bufferUnknownCompletion(state: SessionBgState, completion: BgCompletion): void {
  const now = Date.now();
  pruneUnknownCompletions(state, now);
  state.unknownCompletions = state.unknownCompletions.filter(
    (entry) => entry.completion.task_id !== completion.task_id,
  );
  state.unknownCompletions.push({ completion, receivedAt: now });
  if (state.unknownCompletions.length > UNKNOWN_COMPLETION_CAP) {
    state.unknownCompletions.splice(0, state.unknownCompletions.length - UNKNOWN_COMPLETION_CAP);
  }
}

function pruneUnknownCompletions(state: SessionBgState, now: number): void {
  state.unknownCompletions = state.unknownCompletions.filter(
    (entry) => now - entry.receivedAt <= UNKNOWN_COMPLETION_TTL_MS,
  );
}

function isBgCompletion(value: unknown): value is BgCompletion {
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  const completion = value as Record<string, unknown>;
  return (
    typeof completion.task_id === "string" &&
    typeof completion.status === "string" &&
    (typeof completion.exit_code === "number" || completion.exit_code === null) &&
    typeof completion.command === "string"
  );
}

function appendReminder(output: string, reminder: string): string {
  return output.length > 0 ? `${output}\n\n${reminder}` : reminder;
}

function formatDurationMs(ms: number): string {
  if (!Number.isFinite(ms) || ms < 1000) return `${Math.max(0, Math.round(ms))}ms`;
  const totalSeconds = Math.round(ms / 1000);
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  return minutes > 0 ? `${minutes}m ${seconds}s` : `${seconds}s`;
}

function shorten(value: string, limit: number): string {
  return value.length <= limit ? value : `${value.slice(0, limit - 1)}…`;
}

function formatCompletion(completion: BgCompletion): string {
  const status = formatStatus(completion);
  const duration = formatDuration(completion);
  const header = `- task ${completion.task_id} (${status}${duration ? `, ${duration}` : ""})`;
  const previewBlock = formatOutputPreview(completion);
  return previewBlock ? `${header}\n${previewBlock}` : header;
}

function formatOutputPreview(completion: BgCompletion): string {
  // Strip ANSI escape sequences defensively — most output passes through bash
  // compressors first, but raw stdout from non-compressed commands may still
  // contain colors that bloat the reminder. \x1b is the escape char.
  // biome-ignore lint/suspicious/noControlCharactersInRegex: ANSI escape stripping requires \x1b
  const ansiRegex = /\x1b\[[0-9;]*[a-zA-Z]/g;
  const raw = (completion.output_preview ?? "").replace(ansiRegex, "");
  if (!raw.trim()) return "";
  // Trim trailing newlines so the indented block doesn't end with a blank line
  // but preserve internal newlines so multi-line output stays readable.
  const trimmed = raw.replace(/\n+$/, "");
  const ellipsis = completion.output_truncated ? "…" : "";
  // 4-space indent makes the preview unambiguously a continuation of the
  // bullet above when the agent skims the reminder.
  const indented = trimmed
    .split("\n")
    .map((line) => `    ${line}`)
    .join("\n");
  return ellipsis ? `    ${ellipsis}\n${indented}` : indented;
}

function formatStatus(completion: BgCompletion): string {
  if (completion.status === "timed_out" || completion.status === "timeout") return "timed out";
  if (completion.status === "killed") return "killed";
  if (completion.exit_code !== null) return `exit ${completion.exit_code}`;
  return completion.status;
}

function formatDuration(completion: BgCompletion): string | null {
  const raw = completion.duration_ms ?? completion.runtime_ms ?? completion.runtime;
  if (typeof raw !== "number" || !Number.isFinite(raw) || raw < 0) return null;
  if (raw < 1000) return `${Math.round(raw)}ms`;
  const totalSeconds = Math.round(raw / 1000);
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  return minutes > 0 ? `${minutes}m ${seconds}s` : `${seconds}s`;
}
