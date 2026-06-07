import { createHash, randomUUID } from "node:crypto";
import { sessionDebug, sessionLog, sessionWarn } from "./logger.js";
import { resolvePromptContext } from "./shared/last-assistant-model.js";
import {
  getLiveServerClient,
  setLiveServerWakeAvailable,
  useLiveServerWake,
} from "./shared/live-server-client.js";
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
  mode?: "pipes" | "pty" | string;
  output_path?: string;
}

type ReminderClass = "completion" | "urgent_failure" | "timer" | "pattern_match";

export interface PatternMatchEntry {
  task_id: string;
  session_id: string;
  watch_id: string;
  match_text: string;
  match_offset: number;
  context: string;
  once: boolean;
  reason?: "pattern_match" | "task_exit";
  /** Ack the underlying bash completion after this task-exit reminder is delivered. */
  ackCompletionOnDelivery?: boolean;
}

export interface BgLongRunningReminder {
  task_id: string;
  session_id: string;
  command: string;
  elapsed_ms: number;
  mode?: "pipes" | "pty" | string;
}

type SessionBgState = {
  outstandingTaskIds: Set<string>;
  pendingCompletions: BgCompletion[];
  pendingLongRunning: BgLongRunningReminder[];
  pendingPatternMatches: PatternMatchEntry[];
  explicitControlTasks: Set<string>;
  debounceTimer: NodeJS.Timeout | null;
  firstCompletionAt: number | null;
  scheduledFireAt: number | null;
  scheduledCompletionCount: number;
  scheduledReminderClass: ReminderClass | null;
  retryDelayMs: number | null;
  wakeRetryAttempts: number;
  wakeHardStopped: boolean;
  forcedDrainCompleted: boolean;
  unknownCompletions: Array<{ completion: BgCompletion; receivedAt: number }>;
  /**
   * Task IDs spawned since the last session.idle event. Push completions for
   * these tasks are kept pending but do not promptAsync-wake immediately: the
   * agent may still be in the same assistant turn and about to call sync
   * bash_watch, whose inline result should be the only delivery. In-turn
   * append and the next session.idle still deliver normally.
   */
  wakeDeferredTaskIds: Set<string>;
  deferredCompletionTimer: NodeJS.Timeout | null;
  deferredCompletionDueByTask: Map<string, number>;
  deferredCompletionContext: (DrainContext & { client: unknown }) | null;
  /**
   * Task IDs whose completions were consumed inline by an explicit
   * `bash_status({ exit: true, ... })` wait. The bash_completed push
   * frame for these tasks may arrive AFTER the wait poll loop returned
   * (the Rust→plugin frame is async); without this set, the late frame
   * would land in `pendingCompletions` and the next `appendInTurnBgCompletions`
   * or wake would deliver a duplicate reminder. We dedupe at the ingest
   * boundary so `pendingCompletions` stays a clean source of truth.
   *
   * Bounded by `CONSUMED_TASKIDS_CAP` (FIFO eviction) so a session that
   * runs thousands of bg tasks doesn't grow this set without bound.
   */
  consumedTaskIds: Set<string>;
  consumedTaskOrder: string[];
  lastSeenAt: number;
};

const CONSUMED_TASKIDS_CAP = 256;

export const sessionBgStates: Map<string, SessionBgState> = new Map();

// Lazily evict idle, task-free sessions after 1 hour; no timer is used so the plugin doesn't keep the event loop alive.
export const SESSION_BG_STATE_IDLE_TTL_MS = 60 * 60 * 1000;
const DEBOUNCE_STEP_MS = 200;
const DEBOUNCE_CAP_MS = 1000;
export const DEFERRED_COMPLETION_FALLBACK_MS = 500;
export const WAKE_RETRY_MAX_ATTEMPTS = 5;
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
   * uses this only as fallback when `useLiveServerWake()` is false or when
   * live HTTP wake fails. Preferred wake path is live listener because
   * synchronous `session.prompt(...)` resolution is our delivery proof that
   * OpenCode accepted wake work. Fallback here keeps wakes flowing when live
   * path is unavailable, but `promptAsync` still has false-ack semantics and
   * upstream runner-split bug (anomalyco/opencode#28202; duplicate "stop"
   * messages).
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
  deferredCompletionFallbackMs?: number;
  wakeRetryMaxAttempts?: number;
  wakeDebounceStepMs?: number;
  wakeDebounceCapMs?: number;
}

interface OpenCodeClient {
  session?: {
    prompt?: (input: unknown) => Promise<unknown> | unknown;
    promptAsync?: (input: unknown) => Promise<unknown> | unknown;
    messages?: (input: { path: { id: string } }) => Promise<{ data?: unknown[] }>;
  };
}

interface WakeCorrelationMeta {
  deliveryID: string;
  requestHeaders?: Record<string, string>;
}

type SdkPromptResponseLike = {
  error?: unknown;
  response?: {
    ok?: boolean;
    status?: number;
    statusText?: string;
  };
};

function createWakeCorrelationMeta(
  clientPath: "live-server" | "in-process-fallback",
): WakeCorrelationMeta {
  const deliveryID = `aftdel_${randomUUID()}`;
  return {
    deliveryID,
    requestHeaders:
      clientPath === "live-server"
        ? {
            "x-aft-delivery-id": deliveryID,
          }
        : undefined,
  };
}

function formatWakePromptFailure(result: unknown): string | null {
  if (!result || typeof result !== "object") return null;
  const candidate = result as SdkPromptResponseLike & { request?: unknown };
  const hasError = "error" in candidate && candidate.error != null;
  const response = candidate.response;
  const responseOk = response?.ok;
  const hasBadResponse = responseOk === false;
  if (!hasError && !hasBadResponse) return null;

  const status = typeof response?.status === "number" ? response.status : undefined;
  const statusText = typeof response?.statusText === "string" ? response.statusText : undefined;
  let detail: string | undefined;
  if (typeof candidate.error === "string") {
    detail = candidate.error;
  } else if (candidate.error instanceof Error) {
    detail = candidate.error.message;
  } else if (candidate.error != null) {
    try {
      detail = JSON.stringify(candidate.error);
    } catch {
      detail = String(candidate.error);
    }
  }

  const parts = ["wake prompt returned error"];
  if (status !== undefined) {
    parts.push(`HTTP ${status}${statusText ? ` ${statusText}` : ""}`);
  } else if (statusText) {
    parts.push(statusText);
  }
  if (detail) parts.push(detail);
  return parts.join(": ");
}

/**
 * Mark a bg task's completion as consumed by an explicit bash_status wait.
 * Removes it from pendingCompletions so the next wake/in-turn drain
 * doesn't double-notify the agent.
 */
export function consumeBgCompletion(sessionID: string | undefined, taskId: string): void {
  // Use stateFor (not getSessionState) so the suppression set is recorded
  // even when the session has no prior bg state — the bash_completed push
  // frame for this task may still arrive on this session, and we need the
  // entry there to drop it.
  const state = stateFor(sessionID);
  state.pendingCompletions = state.pendingCompletions.filter((c) => c.task_id !== taskId);
  prunePendingLongRunningForTask(state, taskId);
  resetWakeHardStopForRecovery(state);
  clearDeferredCompletionForTask(state, taskId);
  if (!state.consumedTaskIds.has(taskId)) {
    state.consumedTaskIds.add(taskId);
    state.consumedTaskOrder.push(taskId);
    // Bounded FIFO eviction so a session running thousands of bg tasks
    // doesn't accumulate an unbounded suppression set.
    while (state.consumedTaskOrder.length > CONSUMED_TASKIDS_CAP) {
      const evicted = state.consumedTaskOrder.shift();
      if (evicted !== undefined) state.consumedTaskIds.delete(evicted);
    }
  }
  // Cancel any pending debounced wake when nothing's left to deliver.
  // This closes the race where push frame arrived → scheduleWake →
  // consume removes the only pending entry → wake timer would otherwise
  // fire with empty pending (defensive skip catches that), but firing
  // the timer at all consumes the scheduler slot.
  clearWakeTimerIfNoPending(state);
}

export async function markBgCompletionDelivered(
  drainContext: DrainContext,
  taskId: string,
): Promise<void> {
  await ackCompletions(drainContext, [
    { task_id: taskId, status: "unknown", exit_code: null, command: "" },
  ]);
}

/**
 * Pre-mark a task as expected to be consumed inline before the wait loop
 * starts polling. This is the key suppression mechanism: ingestBgCompletions
 * will skip push frames for tasks already in consumedTaskIds, so a wake is
 * never scheduled in the first place. The consume-after-detection path
 * loses a race when push frame arrives faster than the wait loop's next poll.
 *
 * Caller MUST balance with `unmarkTaskWaiting` if the wait loop returns
 * without seeing terminal status (timeout or pattern-match-without-exit),
 * so future push frames deliver normally.
 */
export function markTaskWaiting(sessionID: string | undefined, taskId: string): void {
  const state = stateFor(sessionID);
  state.pendingCompletions = state.pendingCompletions.filter((c) => c.task_id !== taskId);
  clearDeferredCompletionForTask(state, taskId);
  if (state.consumedTaskIds.has(taskId)) {
    clearWakeTimerIfNoPending(state);
    return;
  }
  state.consumedTaskIds.add(taskId);
  state.consumedTaskOrder.push(taskId);
  while (state.consumedTaskOrder.length > CONSUMED_TASKIDS_CAP) {
    const evicted = state.consumedTaskOrder.shift();
    if (evicted !== undefined) state.consumedTaskIds.delete(evicted);
  }
  // Also drop any pending completion already queued for this task — if
  // ingestBgCompletions ran in the gap between bash() returning task_id
  // and waitForBashStatus calling markTaskWaiting, the completion may
  // already be in pendingCompletions. Filter it out and cancel any wake
  // timer if that empties the queue.
  clearWakeTimerIfNoPending(state);
}

/**
 * Remove a task from the consumed set when the wait loop returned without
 * seeing terminal status (e.g. timeout or pattern-only match). Without
 * this, future push frames for the task would be permanently suppressed.
 */
export function unmarkTaskWaiting(sessionID: string | undefined, taskId: string): void {
  const state = stateFor(sessionID);
  clearDeferredCompletionForTask(state, taskId);
  if (!state.consumedTaskIds.has(taskId)) return;
  state.consumedTaskIds.delete(taskId);
  const idx = state.consumedTaskOrder.indexOf(taskId);
  if (idx >= 0) state.consumedTaskOrder.splice(idx, 1);
}

export function trackBgTask(sessionID: string | undefined, taskId: string): void {
  const state = stateFor(sessionID);
  state.wakeDeferredTaskIds.add(taskId);
  pruneUnknownCompletions(state, Date.now());
  const buffered = state.unknownCompletions.filter((entry) => entry.completion.task_id === taskId);
  state.unknownCompletions = state.unknownCompletions.filter(
    (entry) => entry.completion.task_id !== taskId,
  );
  if (buffered.length > 0) {
    const seenTaskIds = new Set(state.pendingCompletions.map((pending) => pending.task_id));
    for (const entry of buffered) {
      acceptTerminalCompletion(state, entry.completion);
      if (seenTaskIds.has(entry.completion.task_id)) continue;
      state.pendingCompletions.push(entry.completion);
      seenTaskIds.add(entry.completion.task_id);
    }
    scheduleDeferredCompletionFallback(state, taskId);
    return;
  }
  state.outstandingTaskIds.add(taskId);
}

export function markExplicitControl(
  sessionID: string | undefined,
  taskId: string,
  trackOutstanding = true,
): void {
  const state = stateFor(sessionID);
  state.explicitControlTasks.add(taskId);
  if (trackOutstanding) state.outstandingTaskIds.add(taskId);
  // If a push completion already landed for this task before bash_watch
  // could register the explicit control marker, move it from the default
  // pendingCompletions queue (which renders as "[BACKGROUND BASH COMPLETED]")
  // to pendingPatternMatches (which renders as "[BG BASH NOTIFY] task_exit").
  // Without this, both reminders fire because the in-turn-append path drains
  // pendingCompletions regardless of wakeDeferredTaskIds filtering.
  const idx = state.pendingCompletions.findIndex((c) => c.task_id === taskId);
  if (idx >= 0) {
    const completion = state.pendingCompletions[idx];
    state.pendingCompletions.splice(idx, 1);
    acceptTerminalExitPattern(state, completion);
    clearDeferredCompletionForTask(state, taskId);
  }
}

export function unmarkExplicitControl(sessionID: string | undefined, taskId: string): void {
  stateFor(sessionID).explicitControlTasks.delete(taskId);
}

function queuePendingPatternMatch(state: SessionBgState, entry: PatternMatchEntry): void {
  const normalized: PatternMatchEntry = entry.reason
    ? entry
    : { ...entry, reason: "pattern_match" };
  const existingIdx = state.pendingPatternMatches.findIndex(
    (match) => match.task_id === normalized.task_id,
  );
  if (existingIdx >= 0) {
    const existing = state.pendingPatternMatches[existingIdx];
    if (existing.reason !== "pattern_match" && normalized.reason === "pattern_match") {
      state.pendingPatternMatches[existingIdx] = normalized;
    }
    return;
  }
  state.pendingPatternMatches.push(normalized);
}

function routeExplicitControlCompletions(state: SessionBgState): void {
  if (state.pendingCompletions.length === 0) return;
  const remaining: BgCompletion[] = [];
  for (const completion of state.pendingCompletions) {
    if (
      state.explicitControlTasks.has(completion.task_id) ||
      state.pendingPatternMatches.some((match) => match.task_id === completion.task_id)
    ) {
      state.outstandingTaskIds.delete(completion.task_id);
      state.explicitControlTasks.delete(completion.task_id);
      state.wakeDeferredTaskIds.delete(completion.task_id);
      queuePendingPatternMatch(state, completionToExitPattern(completion, true));
    } else {
      remaining.push(completion);
    }
  }
  state.pendingCompletions = remaining;
}

export async function handlePushedPatternMatch(
  drainContext: DrainContext & { client: unknown },
  frame: PatternMatchEntry,
): Promise<void> {
  const state = stateFor(drainContext.sessionID);
  queuePendingPatternMatch(state, frame);
  await triggerWakeIfPending(drainContext, true);
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
    // Suppress completions for tasks already consumed inline by a
    // bash_status wait — the late-arriving frame would otherwise queue
    // a duplicate reminder. We still delete from outstandingTaskIds so
    // tracking stays accurate. See `consumeBgCompletion` for context.
    if (state.consumedTaskIds.has(completion.task_id)) {
      state.outstandingTaskIds.delete(completion.task_id);
      continue;
    }
    if (state.explicitControlTasks.has(completion.task_id)) {
      state.outstandingTaskIds.delete(completion.task_id);
      state.explicitControlTasks.delete(completion.task_id);
      acceptTerminalExitPattern(state, completion);
      continue;
    }
    if (!state.outstandingTaskIds.has(completion.task_id)) {
      bufferUnknownCompletion(state, completion);
      continue;
    }
    state.outstandingTaskIds.delete(completion.task_id);
    acceptTerminalCompletion(state, completion);
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
  const state = stateFor(drainContext.sessionID);
  const taskId = isBgCompletion(completion) ? completion.task_id : undefined;
  sessionDebug(drainContext.sessionID, `${LOG_PREFIX} push completion`, {
    event: "bash_completion_push_ingress",
    kind: "completion",
    task_id: taskId ?? null,
    deferred: taskId ? state.wakeDeferredTaskIds.has(taskId) : null,
    outstanding: taskId ? state.outstandingTaskIds.has(taskId) : null,
  });
  ingestBgCompletions(drainContext.sessionID, [completion]);
  state.deferredCompletionContext = drainContext;
  scheduleDeferredCompletionFallback(state, taskId);
  await triggerWakeIfPending(drainContext, true, false);
}

export async function handlePushedBgLongRunning(
  drainContext: DrainContext & { client: unknown },
  reminder: BgLongRunningReminder,
): Promise<void> {
  const state = stateFor(drainContext.sessionID);
  sessionDebug(drainContext.sessionID, `${LOG_PREFIX} push long-running`, {
    event: "bash_completion_push_ingress",
    kind: "long_running",
    task_id: reminder.task_id,
    deferred: state.wakeDeferredTaskIds.has(reminder.task_id),
    outstanding: state.outstandingTaskIds.has(reminder.task_id),
  });
  state.pendingLongRunning.push(reminder);
  await triggerWakeIfPending(drainContext, true);
}

function resetWakeHardStopForRecovery(state: SessionBgState): void {
  state.wakeHardStopped = false;
  state.wakeRetryAttempts = 0;
  state.retryDelayMs = null;
}

function prunePendingLongRunningForTask(state: SessionBgState, taskId: string): void {
  state.pendingLongRunning = state.pendingLongRunning.filter(
    (reminder) => reminder.task_id !== taskId,
  );
}

function acceptTerminalCompletion(state: SessionBgState, completion: BgCompletion): void {
  prunePendingLongRunningForTask(state, completion.task_id);
  resetWakeHardStopForRecovery(state);
}

function acceptTerminalExitPattern(state: SessionBgState, completion: BgCompletion): void {
  acceptTerminalCompletion(state, completion);
  queuePendingPatternMatch(state, completionToExitPattern(completion, true));
}

export async function appendInTurnBgCompletions(
  drainContext: DrainContext,
  output: { output?: string } | undefined,
): Promise<void> {
  if (!output) return;
  const state = stateFor(drainContext.sessionID);
  promoteBufferedUnknownCompletions(state, Date.now());
  if (
    state.outstandingTaskIds.size === 0 &&
    state.pendingCompletions.length === 0 &&
    state.pendingLongRunning.length === 0 &&
    state.pendingPatternMatches.length === 0
  ) {
    await drainCompletions(drainContext);
    if (
      state.outstandingTaskIds.size === 0 &&
      state.pendingCompletions.length === 0 &&
      state.pendingLongRunning.length === 0 &&
      state.pendingPatternMatches.length === 0
    ) {
      return;
    }
  }

  if (state.outstandingTaskIds.size > 0 || !state.forcedDrainCompleted) {
    await drainCompletions(drainContext);
  }
  routeExplicitControlCompletions(state);
  if (
    state.pendingCompletions.length === 0 &&
    state.pendingLongRunning.length === 0 &&
    state.pendingPatternMatches.length === 0
  )
    return;

  const deliveredCompletions = [...state.pendingCompletions];
  const deliveredPatternMatches = [...state.pendingPatternMatches];
  const completionAcks = completionAcksForDelivery(deliveredCompletions, deliveredPatternMatches);
  const reminder = formatCombinedSystemReminder(
    state.pendingCompletions,
    state.pendingLongRunning,
    state.pendingPatternMatches,
  );
  output.output = appendReminder(output.output ?? "", reminder);
  // Trace #7 of 7: reminder went out as part of an existing tool result
  // instead of through wake client call. NO bash_completion_wake_send_start event
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
  for (const completion of deliveredCompletions) {
    clearDeferredCompletionForTask(state, completion.task_id);
  }
  state.pendingLongRunning = [];
  state.pendingPatternMatches = [];
  state.wakeRetryAttempts = 0;
  state.wakeHardStopped = false;
  await ackCompletions(drainContext, completionAcks);
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
    state.scheduledReminderClass = null;
  }
}

export async function handleIdleBgCompletions(
  drainContext: DrainContext & { client: unknown },
): Promise<void> {
  const state = stateFor(drainContext.sessionID);
  state.deferredCompletionContext = drainContext;
  promoteBufferedUnknownCompletions(state, Date.now());
  state.wakeDeferredTaskIds.clear();
  clearDeferredCompletionState(state);
  await triggerWakeIfPending(drainContext, false, true);
}

async function triggerWakeIfPending(
  drainContext: DrainContext & { client: unknown },
  skipDrain: boolean,
  includeDeferredCompletions = true,
): Promise<void> {
  // Note: previously bailed on `isActive()` (bridge.hasPendingRequests())
  // to defer wakes until the bridge was idle. That was wrong:
  // bridge.hasPendingRequests() returns true for the TUI status RPC poll
  // and any other non-agent traffic. When a bash_completed push arrived
  // during such a window, we'd skip scheduling the wake — and the only
  // recovery paths (session.idle and appendInTurnBgCompletions) can
  // legitimately not fire in time, leaving the agent waiting forever.
  // For tasks spawned in the current assistant turn, wakeDeferredTaskIds
  // suppresses immediate push wakes until either an in-turn append consumes
  // the completion or the next session.idle clears the deferral.
  const state = stateFor(drainContext.sessionID);

  if (!skipDrain && (state.outstandingTaskIds.size > 0 || !state.forcedDrainCompleted)) {
    await drainCompletions(drainContext);
  }
  routeExplicitControlCompletions(state);
  if (!hasWakeEligiblePending(state, includeDeferredCompletions)) {
    const singleKnownTaskId =
      state.pendingCompletions[0]?.task_id ??
      state.pendingLongRunning[0]?.task_id ??
      state.pendingPatternMatches[0]?.task_id ??
      null;
    sessionDebug(drainContext.sessionID, `${LOG_PREFIX} wake skipped; no eligible pending`, {
      event: "bash_completion_wake_no_eligible_pending",
      include_deferred_completions: includeDeferredCompletions,
      pending_completions: state.pendingCompletions.length,
      wake_eligible_completions: wakeEligibleCompletions(state, includeDeferredCompletions).length,
      pending_long_running: state.pendingLongRunning.length,
      pending_pattern_matches: state.pendingPatternMatches.length,
      task_id: singleKnownTaskId,
      deferred: singleKnownTaskId ? state.wakeDeferredTaskIds.has(singleKnownTaskId) : null,
    });
    return;
  }

  scheduleWake(
    state,
    async (reminder, deliveredCompletions) => {
      const taskIDs = deliveredCompletions.map((completion) => completion.task_id);

      const getInProcessClient = (): OpenCodeClient => {
        if (!drainContext.client) {
          sessionWarn(drainContext.sessionID, `${LOG_PREFIX} wake client unavailable`, {
            event: "bash_completion_wake_client_unavailable",
            task_ids: taskIDs,
            directory: drainContext.directory,
            attempt: state.wakeRetryAttempts + 1,
          });
          throw new Error(
            "no wake transport available: live-server unreachable and input.client absent",
          );
        }
        // Cast the unknown `input.client` (real SDK shape with a generated
        // narrower promptAsync signature) to the loose structural shape
        // the wake closure uses. The runtime check in `sendPrompt` confirms
        // shape before use.
        return drainContext.client as OpenCodeClient;
      };

      const sendPrompt = async (
        client: OpenCodeClient,
        clientPath: "live-server" | "in-process-fallback",
        method: "prompt" | "promptAsync",
        correlation: WakeCorrelationMeta = createWakeCorrelationMeta(clientPath),
      ): Promise<string> => {
        const promptMethod = client.session?.[method];
        const session = client.session;
        if (typeof promptMethod !== "function") {
          throw new Error(`wake client.session.${method} is unavailable (path=${clientPath})`);
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

        // Trace #3 of 7: about to call wake client method. The deliveryID uniquely
        // identifies this single wake invocation across the rest of
        // the trace chain (#3 start → #4 ok / #5 error → #6 ack_ok). One
        // deliveryID = one HTTP POST to OpenCode's session prompt endpoint.
        // When the DB shows multiple assistant children but logs show one
        // start event with this deliveryID, the duplication is downstream
        // of AFT.
        const { deliveryID, requestHeaders } = correlation;
        const wakeMeta = {
          delivery_id: deliveryID,
          correlation_header:
            requestHeaders && Object.keys(requestHeaders).length > 0
              ? Object.keys(requestHeaders)[0]
              : null,
          attempt: state.wakeRetryAttempts + 1,
          task_ids: taskIDs,
          directory: drainContext.directory,
          reminder_sha256: hashReminder(reminder),
          reminder_chars: reminder.length,
          // `live-server` = wake sent through `createOpencodeClient` aimed at
          // `input.serverUrl`; `prompt` resolution on this path is delivery
          // proof that live listener accepted prompt. `in-process-fallback` =
          // wake sent through plugin-provided client because live listener was
          // unavailable or failed mid-session; this path prefers sync `prompt`
          // too, but may degrade to `promptAsync` when older client shape lacks it.
          wake_client_path: clientPath,
          wake_client_method: method,
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
        sessionLog(drainContext.sessionID, `${LOG_PREFIX} wake send start`, {
          event: "bash_completion_wake_send_start",
          ...wakeMeta,
        });
        try {
          const result = await promptMethod.call(session, {
            path: { id: drainContext.sessionID },
            body,
            throwOnError: true,
          });
          const sdkFailure = formatWakePromptFailure(result);
          if (sdkFailure) throw new Error(sdkFailure);
        } catch (err) {
          // Trace #5 of 7: wake client method rejected. Counted toward
          // wake_retry_max_attempts by the catch in scheduleWake unless a
          // live-server failure can be delivered by the in-process fallback
          // below. Re-throw so the retry/fallback path runs.
          const logPromptError = clientPath === "live-server" ? sessionDebug : sessionWarn;
          logPromptError(drainContext.sessionID, `${LOG_PREFIX} wake send error`, {
            event: "bash_completion_wake_send_error",
            delivery_id: deliveryID,
            attempt: state.wakeRetryAttempts + 1,
            task_ids: taskIDs,
            wake_client_path: clientPath,
            wake_client_method: method,
            error: err instanceof Error ? err.message : String(err),
          });
          throw err;
        }
        // Trace #4 of 7: wake client method resolved. For live-server `prompt`,
        // this is delivery proof that OpenCode accepted wake on live listener.
        // For degraded `promptAsync`, resolution is weaker transport-only proof.
        sessionLog(drainContext.sessionID, `${LOG_PREFIX} wake send resolved`, {
          event: "bash_completion_wake_send_ok",
          delivery_id: deliveryID,
          attempt: state.wakeRetryAttempts + 1,
          task_ids: taskIDs,
          wake_client_path: clientPath,
          wake_client_method: method,
        });
        if (method === "promptAsync") {
          sessionWarn(drainContext.sessionID, `${LOG_PREFIX} wake degraded delivery`, {
            event: "bash_completion_wake_degraded_delivery",
            delivery_id: deliveryID,
            attempt: state.wakeRetryAttempts + 1,
            task_ids: taskIDs,
            wake_client_path: clientPath,
            wake_client_method: method,
          });
        }
        return deliveryID;
      };

      // Wake transport selection is keyed by serverUrl. A reachable live
      // server gets the anomalyco/opencode#28202 workaround; otherwise we
      // fall back to the plugin-provided in-process client. If the live
      // server fails after an earlier successful probe, demote that cached
      // serverUrl decision and retry this same delivery through the
      // in-process client before spending the scheduler retry budget.
      if (useLiveServerWake(drainContext.serverUrl) && drainContext.serverUrl) {
        try {
          const { deliveryID, requestHeaders } = createWakeCorrelationMeta("live-server");
          const liveClient = getLiveServerClient(
            drainContext.serverUrl,
            drainContext.directory,
            requestHeaders,
          ) as OpenCodeClient;
          const deliveryIDResolved = await sendPrompt(liveClient, "live-server", "prompt", {
            deliveryID,
            requestHeaders,
          });
          if (deliveryIDResolved !== deliveryID) {
            sessionDebug(drainContext.sessionID, `${LOG_PREFIX} delivery correlation mismatch`, {
              event: "bash_completion_wake_delivery_correlation_mismatch",
              expected_delivery_id: deliveryID,
              actual_delivery_id: deliveryIDResolved,
              wake_client_path: "live-server",
            });
          }
          await ackCompletions(drainContext, deliveredCompletions, deliveryIDResolved);
          return;
        } catch (err) {
          setLiveServerWakeAvailable(drainContext.serverUrl, false);
          // Falling back from live-server to the in-process client is the
          // expected safe path when the optional duplicate-runner workaround is
          // unavailable. Keep it DEBUG; the scheduler emits WARN only if no
          // transport ultimately delivers the wake.
          sessionDebug(
            drainContext.sessionID,
            `${LOG_PREFIX} live-server wake failed; falling back`,
            {
              event: "bash_completion_wake_live_server_fallback",
              task_ids: taskIDs,
              directory: drainContext.directory,
              server_url: drainContext.serverUrl,
              attempt: state.wakeRetryAttempts + 1,
              error: err instanceof Error ? err.message : String(err),
            },
          );
          const fallbackClient = getInProcessClient();
          const fallbackMethod =
            typeof fallbackClient.session?.prompt === "function" ? "prompt" : "promptAsync";
          const deliveryID = await sendPrompt(
            fallbackClient,
            "in-process-fallback",
            fallbackMethod,
          );
          // This delivery succeeded by switching transports; do not carry
          // over retry attempts spent on the now-demoted live-server path.
          state.retryDelayMs = null;
          state.wakeRetryAttempts = 0;
          state.wakeHardStopped = false;
          await ackCompletions(drainContext, deliveredCompletions, deliveryID);
          return;
        }
      }

      const fallbackClient = getInProcessClient();
      const fallbackMethod =
        typeof fallbackClient.session?.prompt === "function" ? "prompt" : "promptAsync";
      const deliveryID = await sendPrompt(fallbackClient, "in-process-fallback", fallbackMethod);
      await ackCompletions(drainContext, deliveredCompletions, deliveryID);
    },
    (err, hardStopped) => {
      sessionWarn(
        drainContext.sessionID,
        hardStopped
          ? `${LOG_PREFIX} wake send failed ${(drainContext.wakeRetryMaxAttempts ?? WAKE_RETRY_MAX_ATTEMPTS)} times; stopping retries: ${err instanceof Error ? err.message : String(err)}`
          : `${LOG_PREFIX} wake send failed: ${err instanceof Error ? err.message : String(err)}`,
      );
    },
    drainContext.sessionID,
    includeDeferredCompletions,
    drainContext.wakeRetryMaxAttempts ?? WAKE_RETRY_MAX_ATTEMPTS,
    drainContext.wakeDebounceStepMs ?? DEBOUNCE_STEP_MS,
    drainContext.wakeDebounceCapMs ?? DEBOUNCE_CAP_MS,
  );
}

export function formatSystemReminder(completions: readonly BgCompletion[]): string {
  const urgent = completions.filter(isUrgentCompletion);
  const normal = completions.filter((completion) => !isUrgentCompletion(completion));
  const sections: string[] = [];
  if (urgent.length > 0) sections.push(renderCompletionSection("BACKGROUND BASH FAILED", urgent));
  if (normal.length > 0)
    sections.push(renderCompletionSection("BACKGROUND BASH COMPLETED", normal));
  return sections.join("\n");
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

export function formatPatternMatchReminder(matches: readonly PatternMatchEntry[]): string {
  const bullets = matches
    .map((match) => {
      const context = (match.context || match.match_text).replace(/\n/g, "\n      > ");
      if (match.reason === "task_exit") {
        return `- task ${match.task_id} exited:\n      > ${context}`;
      }
      return `- task ${match.task_id} matched ${JSON.stringify(match.match_text)} (offset ${match.match_offset}):\n      > ${context}`;
    })
    .join("\n");
  return `<system-reminder>\n[BG BASH NOTIFY]\n${bullets}\n</system-reminder>`;
}

function formatCombinedSystemReminder(
  completions: readonly BgCompletion[],
  longRunning: readonly BgLongRunningReminder[],
  patternMatches: readonly PatternMatchEntry[] = [],
): string {
  const parts: string[] = [];
  if (completions.length > 0) parts.push(formatSystemReminder(completions));
  if (longRunning.length > 0) parts.push(formatLongRunningReminder(longRunning));
  if (patternMatches.length > 0) parts.push(formatPatternMatchReminder(patternMatches));
  return parts.join("\n");
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
    if (state.deferredCompletionTimer) clearTimeout(state.deferredCompletionTimer);
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
    const accepted = ingestDrainedBgCompletions(sessionID, response.bg_completions);
    sessionDebug(sessionID, `${LOG_PREFIX} drain ok`, {
      event: "bash_completion_drain_ok",
      accepted_count: accepted.length,
      accepted_task_ids: accepted.map((completion) => completion.task_id),
    });
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

function hasWakeEligiblePending(
  state: SessionBgState,
  includeDeferredCompletions: boolean,
): boolean {
  return (
    wakeEligibleCompletions(state, includeDeferredCompletions).length > 0 ||
    state.pendingLongRunning.length > 0 ||
    state.pendingPatternMatches.length > 0
  );
}

function wakeEligibleCompletions(
  state: SessionBgState,
  includeDeferredCompletions: boolean,
): BgCompletion[] {
  if (includeDeferredCompletions || state.wakeDeferredTaskIds.size === 0) {
    return state.pendingCompletions;
  }
  return state.pendingCompletions.filter(
    (completion) => !state.wakeDeferredTaskIds.has(completion.task_id),
  );
}

function clearWakeTimerIfNoPending(state: SessionBgState): void {
  if (
    state.pendingCompletions.length === 0 &&
    state.pendingLongRunning.length === 0 &&
    state.pendingPatternMatches.length === 0 &&
    state.debounceTimer
  ) {
    clearTimeout(state.debounceTimer);
    state.debounceTimer = null;
    state.firstCompletionAt = null;
    state.scheduledFireAt = null;
    state.scheduledCompletionCount = 0;
    state.scheduledReminderClass = null;
  }
  if (state.pendingCompletions.length === 0) clearDeferredCompletionState(state);
}

function clearDeferredCompletionTimer(state: SessionBgState): void {
  if (state.deferredCompletionTimer) clearTimeout(state.deferredCompletionTimer);
  state.deferredCompletionTimer = null;
}

function clearDeferredCompletionState(state: SessionBgState): void {
  clearDeferredCompletionTimer(state);
  state.deferredCompletionDueByTask.clear();
}

function clearDeferredCompletionForTask(state: SessionBgState, taskId: string): void {
  state.wakeDeferredTaskIds.delete(taskId);
  state.deferredCompletionDueByTask.delete(taskId);
  if (state.deferredCompletionDueByTask.size === 0) {
    clearDeferredCompletionTimer(state);
    return;
  }
  scheduleDeferredCompletionFallback(state);
}

function scheduleDeferredCompletionFallback(
  state: SessionBgState,
  taskId?: string,
  now = Date.now(),
): void {
  if (taskId) {
    const pending = state.pendingCompletions.some((completion) => completion.task_id === taskId);
    if (!pending || !state.wakeDeferredTaskIds.has(taskId)) {
      state.deferredCompletionDueByTask.delete(taskId);
    } else {
      const fallbackMs =
        state.deferredCompletionContext?.deferredCompletionFallbackMs ??
        DEFERRED_COMPLETION_FALLBACK_MS;
      state.deferredCompletionDueByTask.set(taskId, now + fallbackMs);
    }
  }

  for (const dueTaskId of [...state.deferredCompletionDueByTask.keys()]) {
    if (
      !state.wakeDeferredTaskIds.has(dueTaskId) ||
      !state.pendingCompletions.some((completion) => completion.task_id === dueTaskId)
    ) {
      state.deferredCompletionDueByTask.delete(dueTaskId);
    }
  }

  if (state.deferredCompletionDueByTask.size === 0 || !state.deferredCompletionContext) {
    clearDeferredCompletionTimer(state);
    return;
  }

  const nextDueAt = Math.min(...state.deferredCompletionDueByTask.values());
  const delay = Math.max(0, nextDueAt - now);
  clearDeferredCompletionTimer(state);
  state.deferredCompletionTimer = setTimeout(() => {
    const fireNow = Date.now();
    const maturedTaskIds: string[] = [];
    for (const [dueTaskId, dueAt] of state.deferredCompletionDueByTask) {
      if (dueAt <= fireNow) maturedTaskIds.push(dueTaskId);
    }
    for (const maturedTaskId of maturedTaskIds) {
      state.deferredCompletionDueByTask.delete(maturedTaskId);
      state.wakeDeferredTaskIds.delete(maturedTaskId);
    }
    if (state.deferredCompletionDueByTask.size === 0) {
      state.deferredCompletionTimer = null;
    } else {
      scheduleDeferredCompletionFallback(state, undefined, fireNow);
    }
    if (maturedTaskIds.length === 0) return;
    const context = state.deferredCompletionContext;
    if (!context) return;
    void triggerWakeIfPending(context, true, false);
  }, delay);
}

function scheduleWake(
  state: SessionBgState,
  sendWake: (reminder: string, completions: readonly BgCompletion[]) => Promise<void>,
  onSendFailure: (err: unknown, hardStopped: boolean) => void,
  sessionID?: string,
  includeDeferredCompletions = true,
  maxWakeSendAttempts = WAKE_RETRY_MAX_ATTEMPTS,
  debounceStepMs = DEBOUNCE_STEP_MS,
  debounceCapMs = DEBOUNCE_CAP_MS,
): void {
  if (state.wakeHardStopped) {
    sessionDebug(sessionID, `${LOG_PREFIX} wake hard-stopped`, {
      event: "bash_completion_wake_hard_stopped",
      pending_completions: state.pendingCompletions.length,
      pending_long_running: state.pendingLongRunning.length,
      pending_pattern_matches: state.pendingPatternMatches.length,
    });
    return;
  }
  // Race model: JS state changes are synchronous; awaits only happen before scheduling
  // drains and during final prompt delivery. Multiple hook invocations can interleave
  // only at those awaits, so we gate timer extension on the pending completion count.
  const now = Date.now();
  const pendingCount =
    wakeEligibleCompletions(state, includeDeferredCompletions).length +
    state.pendingLongRunning.length +
    state.pendingPatternMatches.length;
  const reminderClass = reminderClassForPending(state, includeDeferredCompletions);
  if (!reminderClass) return;
  if (
    state.debounceTimer &&
    pendingCount <= state.scheduledCompletionCount &&
    reminderPriority(reminderClass) <= reminderPriority(state.scheduledReminderClass)
  ) {
    return;
  }
  if (state.firstCompletionAt === null) {
    state.firstCompletionAt = now;
    state.scheduledFireAt = now + debounceDelayForReminderClass(reminderClass, debounceStepMs);
  } else {
    const previousFireAt = state.scheduledFireAt ?? now;
    state.scheduledFireAt =
      reminderClass === "urgent_failure"
        ? now
        : Math.min(previousFireAt + debounceStepMs, state.firstCompletionAt + debounceCapMs);
  }
  state.scheduledCompletionCount = pendingCount;
  state.scheduledReminderClass = reminderClass;

  if (state.debounceTimer) clearTimeout(state.debounceTimer);
  const delay = state.retryDelayMs ?? Math.max(0, (state.scheduledFireAt ?? now) - now);

  // Trace #1 of 7 for the wake-delivery chain. Pairs with bash_completion_wake_fire.
  // When the OpenCode DB later shows N assistant children for one parent
  // user message, the matching count of wake_scheduled / wake_fire /
  // wake_send_start events for same task_ids tells us whether
  // AFT submitted the prompt once or N times. See
  // .alfonso/incident-reports/2026-05-21-bash-reminder-duplicate-runs.md.
  sessionLog(sessionID, `${LOG_PREFIX} wake scheduled`, {
    event: "bash_completion_wake_scheduled",
    delay_ms: delay,
    pending_completions: state.pendingCompletions.length,
    pending_long_running: state.pendingLongRunning.length,
    pending_pattern_matches: state.pendingPatternMatches.length,
    retry_attempt: state.wakeRetryAttempts,
  });

  state.debounceTimer = setTimeout(() => {
    const pending = wakeEligibleCompletions(state, includeDeferredCompletions);
    const pendingLongRunning = state.pendingLongRunning;
    const pendingPatternMatches = state.pendingPatternMatches;
    state.debounceTimer = null;
    state.firstCompletionAt = null;
    state.scheduledFireAt = null;
    state.scheduledCompletionCount = 0;
    state.scheduledReminderClass = null;
    // Defensive: if another path (e.g. appendInTurnBgCompletions) drained the
    // pending arrays between schedule and fire and didn't cancel us, just
    // skip — don't ship an empty "[BACKGROUND BASH STILL RUNNING]" shell.
    if (
      pending.length === 0 &&
      pendingLongRunning.length === 0 &&
      pendingPatternMatches.length === 0
    )
      return;
    const reminder = formatCombinedSystemReminder(
      pending,
      pendingLongRunning,
      pendingPatternMatches,
    );

    // Trace #2 of 7: timer actually fired and we captured a non-empty
    // pending set. Matching wake_send_start MUST follow within
    // ~milliseconds — absence means sendWake threw synchronously
    // before reaching client.session.prompt / promptAsync.
    sessionLog(sessionID, `${LOG_PREFIX} wake fire`, {
      event: "bash_completion_wake_fire",
      task_ids: pending.map((c) => c.task_id),
      long_running_task_ids: pendingLongRunning.map((r) => r.task_id),
      reminder_sha256: hashReminder(reminder),
      reminder_chars: reminder.length,
      retry_attempt: state.wakeRetryAttempts,
    });

    const deliveredTaskIds = new Set(pending.map((completion) => completion.task_id));
    const longRunningTaskIds = new Set(pendingLongRunning.map((reminder) => reminder.task_id));
    state.pendingCompletions = state.pendingCompletions.filter(
      (completion) => !deliveredTaskIds.has(completion.task_id),
    );
    for (const taskId of deliveredTaskIds) clearDeferredCompletionForTask(state, taskId);
    for (const taskId of longRunningTaskIds) clearDeferredCompletionForTask(state, taskId);
    state.pendingLongRunning = [];
    state.pendingPatternMatches = [];
    const completionAcks = completionAcksForDelivery(pending, pendingPatternMatches);
    void sendWake(reminder, completionAcks)
      .then(() => {
        state.retryDelayMs = null;
        state.wakeRetryAttempts = 0;
        state.wakeHardStopped = false;
        state.scheduledReminderClass = null;
      })
      .catch((err) => {
        state.pendingCompletions = [...pending, ...state.pendingCompletions];
        state.pendingLongRunning = [...pendingLongRunning, ...state.pendingLongRunning];
        state.pendingPatternMatches = [...pendingPatternMatches, ...state.pendingPatternMatches];
        state.wakeRetryAttempts += 1;
        if (state.wakeRetryAttempts >= maxWakeSendAttempts) {
          state.retryDelayMs = null;
          state.wakeHardStopped = true;
          onSendFailure(err, true);
          return;
        }
        state.retryDelayMs = Math.min((delay || debounceStepMs) * 2, debounceCapMs);
        onSendFailure(err, false);
        scheduleWake(
          state,
          sendWake,
          onSendFailure,
          sessionID,
          includeDeferredCompletions,
          maxWakeSendAttempts,
          debounceStepMs,
          debounceCapMs,
        );
      });
  }, delay);
}

function _getSessionState(sessionID: string | undefined): SessionBgState | undefined {
  cleanupIdleSessionStates(Date.now());
  return sessionBgStates.get(sessionID || DEFAULT_SESSION_ID);
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
      pendingPatternMatches: [],
      explicitControlTasks: new Set(),
      debounceTimer: null,
      firstCompletionAt: null,
      scheduledFireAt: null,
      scheduledCompletionCount: 0,
      scheduledReminderClass: null,
      retryDelayMs: null,
      wakeRetryAttempts: 0,
      wakeHardStopped: false,
      forcedDrainCompleted: false,
      unknownCompletions: [],
      wakeDeferredTaskIds: new Set(),
      deferredCompletionTimer: null,
      deferredCompletionDueByTask: new Map(),
      deferredCompletionContext: null,
      consumedTaskIds: new Set(),
      consumedTaskOrder: [],
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
    if (state.explicitControlTasks.has(completion.task_id)) {
      state.explicitControlTasks.delete(completion.task_id);
      acceptTerminalExitPattern(state, completion);
      continue;
    }
    // Suppress completions for tasks already consumed inline by a
    // bash_status wait (same dedupe as ingestBgCompletions push path).
    if (state.consumedTaskIds.has(completion.task_id)) continue;
    acceptTerminalCompletion(state, completion);
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
    clearDeferredCompletionState(state);
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

function promoteBufferedUnknownCompletions(state: SessionBgState, now: number): void {
  pruneUnknownCompletions(state, now);
  if (state.unknownCompletions.length === 0) return;

  const remaining: Array<{ completion: BgCompletion; receivedAt: number }> = [];
  const promoted: BgCompletion[] = [];
  const seenTaskIds = new Set(state.pendingCompletions.map((pending) => pending.task_id));

  for (const entry of state.unknownCompletions) {
    const completion = entry.completion;
    state.outstandingTaskIds.delete(completion.task_id);

    if (state.consumedTaskIds.has(completion.task_id)) continue;

    if (state.explicitControlTasks.has(completion.task_id)) {
      state.explicitControlTasks.delete(completion.task_id);
      acceptTerminalExitPattern(state, completion);
      continue;
    }

    acceptTerminalCompletion(state, completion);
    if (seenTaskIds.has(completion.task_id)) continue;
    promoted.push(completion);
    seenTaskIds.add(completion.task_id);
  }

  state.unknownCompletions = remaining;
  if (promoted.length > 0) state.pendingCompletions.push(...promoted);
}

function completionToExitPattern(
  completion: BgCompletion,
  ackCompletionOnDelivery = false,
): PatternMatchEntry {
  const status = formatStatus(completion);
  const preview = formatOutputPreview(completion).replace(/^ {4}/gm, "").slice(-300);
  const entry: PatternMatchEntry = {
    task_id: completion.task_id,
    session_id: "",
    watch_id: "exit",
    match_text: "",
    match_offset: 0,
    context: preview
      ? `task ${completion.task_id} exited (${status})\n${preview}`
      : `task ${completion.task_id} exited (${status})`,
    once: true,
    reason: "task_exit",
  };
  if (ackCompletionOnDelivery) entry.ackCompletionOnDelivery = true;
  return entry;
}

function completionAcksForDelivery(
  completions: readonly BgCompletion[],
  patternMatches: readonly PatternMatchEntry[],
): BgCompletion[] {
  const acks = [...completions];
  const ackedTaskIds = new Set(acks.map((completion) => completion.task_id));
  for (const match of patternMatches) {
    if (!match.ackCompletionOnDelivery || ackedTaskIds.has(match.task_id)) continue;
    acks.push({ task_id: match.task_id, status: "unknown", exit_code: null, command: "" });
    ackedTaskIds.add(match.task_id);
  }
  return acks;
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

function isUrgentCompletion(completion: BgCompletion): boolean {
  return ["failed", "timed_out", "timeout", "killed"].includes(completion.status);
}

function renderCompletionSection(header: string, completions: readonly BgCompletion[]): string {
  const bullets = completions.map((completion) => formatCompletion(completion)).join("\n");
  const anyTruncated = completions.some((c) => c.output_truncated === true);
  const tail = anyTruncated
    ? `\n\nFor truncated tasks, use bash_status({ taskId: "..." }) to retrieve full output.`
    : "";
  return `<system-reminder>\n[${header}]\n${bullets}${tail}\n</system-reminder>`;
}

function reminderClassForPending(
  state: SessionBgState,
  includeDeferredCompletions: boolean,
): ReminderClass | null {
  const completions = wakeEligibleCompletions(state, includeDeferredCompletions);
  if (completions.some(isUrgentCompletion)) return "urgent_failure";
  if (state.pendingLongRunning.length > 0) return "timer";
  if (completions.length > 0) return "completion";
  if (state.pendingPatternMatches.length > 0) return "pattern_match";
  return null;
}

function reminderPriority(reminderClass: ReminderClass | null): number {
  switch (reminderClass) {
    case "urgent_failure":
      return 3;
    case "timer":
      return 2;
    case "completion":
    case "pattern_match":
      return 1;
    default:
      return 0;
  }
}

function debounceDelayForReminderClass(
  reminderClass: ReminderClass,
  debounceStepMs = DEBOUNCE_STEP_MS,
): number {
  return reminderClass === "urgent_failure" ? 0 : debounceStepMs;
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
