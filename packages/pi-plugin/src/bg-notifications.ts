import {
  type AftProjectTransport,
  type BgNudgeRef,
  resolveBridgeForNudge,
} from "@cortexkit/aft-bridge";
import { sessionLog, sessionWarn } from "./logger.js";
import type { PluginContext } from "./types.js";

export interface BgCompletion {
  task_id: string;
  status: string;
  exit_code: number | null;
  command: string;
  duration_ms?: number;
  runtime_ms?: number;
  runtime?: number;
  /**
   * Exit-aware preview of stdout+stderr captured at completion (from Rust):
   * success = short tail (≤600 B), failure = small head + larger tail
   * (≤2.25 KiB). Full output stays recoverable via bash_status / file pointers.
   */
  output_preview?: string;
  /** True when the captured tail is shorter than the actual output. */
  output_truncated?: boolean;
  // Token counts arrive in v0.27 but commit 7 leaves them unused.
  // Commit 13 will write them to storage via aft_db_record_compression.
  original_tokens?: number;
  compressed_tokens?: number;
  tokens_skipped?: boolean;
  status_reason?: string;
  mode?: "pipes" | "pty" | string;
  output_path?: string;
}

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
  retryDelayMs: number | null;
  wakeRetryAttempts: number;
  wakeHardStopped: boolean;
  forcedDrainCompleted: boolean;
  unknownCompletions: Array<{ completion: BgCompletion; receivedAt: number }>;
  /**
   * Task IDs spawned since the last session idle boundary. Push completions
   * for these tasks stay pending but do not send an immediate follow-up;
   * sync bash_watch may still consume them inline in the same turn.
   */
  wakeDeferredTaskIds: Set<string>;
  /**
   * Task IDs whose completions were consumed inline by an explicit
   * `bash_status({ exit: true, ... })` wait. The bash_completed push
   * frame for these tasks may arrive AFTER the wait poll loop returned;
   * without this set, the late frame would land in pendingCompletions
   * and the next drain would deliver a duplicate reminder. We dedupe
   * at the ingest boundary so pendingCompletions stays a clean source
   * of truth. Bounded FIFO at CONSUMED_TASKIDS_CAP.
   */
  consumedTaskIds: Set<string>;
  consumedTaskOrder: string[];
  /**
   * Task IDs whose delivery is IN FLIGHT (removed from pendingCompletions /
   * pattern queues, delivery not yet resolved). Ingest skips these so a subc
   * forced drain in the removal→delivery window can't re-accept and double-deliver
   * an already-departing completion (audit C-#1). Mirrors the OpenCode twin.
   */
  deliveringTaskIds: Set<string>;
  /**
   * Task IDs DELIVERED to the agent but whose `bash_ack_completions` has not yet
   * confirmed (Rust still holds them and, over subc, re-nudges). Ingest skips them
   * for fresh delivery AND a forced drain RE-ACKs them (the self-terminating close
   * of the re-nudge loop, C-#3). Removed by DAEMON RECONCILIATION, not a timer:
   * when a forced drain no longer returns a task as unacknowledged, it is safe to
   * forget (R2-T3 — a time TTL could evict a still-held task and reopen the
   * double-deliver). Insertion-ordered for the FIFO OOM backstop cap.
   */
  deliveredAwaitingAckTaskIds: Set<string>;
  lastSeenAt: number;
};

const CONSUMED_TASKIDS_CAP = 256;
/**
 * Pure OOM backstop for deliveredAwaitingAckTaskIds (drain reconciliation is the
 * real bound — this should never realistically fire). Set far above any plausible
 * in-flight set; evicting a still-held entry could reopen the double-deliver.
 */
const DELIVERED_AWAITING_ACK_CAP = 4096;

type TextContent = { type: "text"; text: string; textSignature?: string };
type ImageContent = { type: "image"; data: string; mimeType: string };
type ContentBlock = TextContent | ImageContent;
type SendUserMessageRuntime = {
  sendUserMessage: (content: string, options?: { deliverAs?: "steer" | "followUp" }) => void;
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
const LOG_PREFIX = "[aft-pi] bg-notifications:";
const SUBC_NUDGE_LOG_INTERVAL_MS = 60_000;
const DEFAULT_BG_HOP_TIMEOUT_MS = 15_000;
let bgHopTimeoutMs = DEFAULT_BG_HOP_TIMEOUT_MS;
const subcNudgesInFlight = new Map<string, Promise<void>>();
const subcNudgeLogState = new Map<string, { lastEmittedAt: number; suppressed: number }>();

interface DrainContext {
  ctx: PluginContext;
  directory: string;
  sessionID?: string;
  /** Complete provenance for a subc bg_events nudge. */
  nudgeRef?: BgNudgeRef;
  /** Cached bridge so one nudge resolves drain and ack through one authority. */
  resolvedBridge?: AftProjectTransport;
}

type ResolvedDrainContext = DrainContext & { sessionID: string };

function normalizeSessionID(sessionID: string | undefined): string {
  return sessionID && sessionID.length > 0 ? sessionID : DEFAULT_SESSION_ID;
}

/**
 * Mark a bg task's completion as consumed by an explicit bash_status wait.
 * Removes it from pendingCompletions so the next wake/in-turn drain
 * doesn't double-notify the agent.
 */
export function consumeBgCompletion(sessionID: string | undefined, taskId: string): void {
  // Use stateFor (not getSessionState) so the suppression set is recorded
  // even when the session has no prior bg state — the bash_completed push
  // frame may still arrive on this session and we need the entry there
  // to drop it. Mirrors the OpenCode fix.
  const state = stateFor(sessionID);
  state.pendingCompletions = state.pendingCompletions.filter((c) => c.task_id !== taskId);
  state.wakeDeferredTaskIds.delete(taskId);
  if (!state.consumedTaskIds.has(taskId)) {
    state.consumedTaskIds.add(taskId);
    state.consumedTaskOrder.push(taskId);
    while (state.consumedTaskOrder.length > CONSUMED_TASKIDS_CAP) {
      const evicted = state.consumedTaskOrder.shift();
      if (evicted !== undefined) state.consumedTaskIds.delete(evicted);
    }
  }
  // Cancel any pending debounced wake when nothing's left to deliver.
  clearWakeTimerIfNoPending(state);
}

export async function markBgCompletionDelivered(
  drainContext: DrainContext,
  taskId: string,
): Promise<void> {
  // C-#1 site 5: an inline bash_watch wait already delivered this task's result.
  // Mark it awaiting-ack so a forced drain racing this ack re-acks rather than
  // re-delivering, then confirm/keep on the ack result. (Not via consumedTaskIds —
  // that set is also used pre-delivery by markTaskWaiting.)
  const state = stateFor(drainContext.sessionID);
  markDeliveredAwaitingAck(state, [taskId]);
  const acked = await ackCompletions(drainContext, [
    { task_id: taskId, status: "unknown", exit_code: null, command: "" },
  ]);
  if (acked) confirmAcked(state, [taskId]);
}

/**
 * Pre-mark a task as expected to be consumed inline before the wait loop
 * starts polling. See OpenCode `markTaskWaiting` for full design notes.
 */
export function markTaskWaiting(sessionID: string | undefined, taskId: string): void {
  const state = stateFor(sessionID);
  state.pendingCompletions = state.pendingCompletions.filter((c) => c.task_id !== taskId);
  state.wakeDeferredTaskIds.delete(taskId);
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
  clearWakeTimerIfNoPending(state);
}

/**
 * Remove a task from the consumed set when the wait loop returned without
 * seeing terminal status. Without this, future push frames would be
 * permanently suppressed.
 */
export function unmarkTaskWaiting(sessionID: string | undefined, taskId: string): void {
  const state = stateFor(sessionID);
  state.wakeDeferredTaskIds.delete(taskId);
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
    for (const entry of buffered) {
      if (!state.pendingCompletions.some((pending) => pending.task_id === taskId)) {
        state.pendingCompletions.push(entry.completion);
      }
    }
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
    queuePendingPatternMatch(state, completionToExitPattern(completion, true));
    state.wakeDeferredTaskIds.delete(taskId);
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
  drainContext: DrainContext & { runtime: SendUserMessageRuntime },
  frame: PatternMatchEntry,
): Promise<void> {
  const state = stateFor(drainContext.sessionID);
  queuePendingPatternMatch(state, { ...frame, ackCompletionOnDelivery: true });
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
      queuePendingPatternMatch(state, completionToExitPattern(completion, true));
      continue;
    }
    // C-#1: skip a task whose delivery is in flight / awaiting ack — a late push
    // for an already-departing completion would otherwise re-queue a duplicate.
    if (isDeliveringOrAwaitingAck(state, completion.task_id)) {
      state.outstandingTaskIds.delete(completion.task_id);
      continue;
    }
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
  drainContext: DrainContext & { runtime: SendUserMessageRuntime },
  completion: unknown,
): Promise<void> {
  ingestBgCompletions(drainContext.sessionID, [completion]);
  await triggerWakeIfPending(drainContext, true, false);
}

export async function handlePushedBgLongRunning(
  drainContext: DrainContext & { runtime: SendUserMessageRuntime },
  reminder: BgLongRunningReminder,
): Promise<void> {
  stateFor(drainContext.sessionID).pendingLongRunning.push(reminder);
  await triggerWakeIfPending(drainContext, true);
}

export async function appendToolResultBgCompletions(
  drainContext: DrainContext,
  content: ContentBlock[],
): Promise<ContentBlock[] | undefined> {
  const state = stateFor(drainContext.sessionID);
  if (
    state.outstandingTaskIds.size === 0 &&
    state.pendingCompletions.length === 0 &&
    state.pendingLongRunning.length === 0 &&
    state.pendingPatternMatches.length === 0
  )
    await drainCompletions(drainContext);
  if (
    state.outstandingTaskIds.size === 0 &&
    state.pendingCompletions.length === 0 &&
    state.pendingLongRunning.length === 0 &&
    state.pendingPatternMatches.length === 0
  )
    return undefined;

  if (state.outstandingTaskIds.size > 0 || !state.forcedDrainCompleted) {
    await drainCompletions(drainContext);
  }
  routeExplicitControlCompletions(state);
  if (
    state.pendingCompletions.length === 0 &&
    state.pendingLongRunning.length === 0 &&
    state.pendingPatternMatches.length === 0
  )
    return undefined;

  const deliveredCompletions = [...state.pendingCompletions];
  const deliveredPatternMatches = [...state.pendingPatternMatches];
  const completionAcks = completionAcksForDelivery(deliveredCompletions, deliveredPatternMatches);
  const reminder = formatCombinedSystemReminder(
    state.pendingCompletions,
    state.pendingLongRunning,
    state.pendingPatternMatches,
  );
  state.pendingCompletions = [];
  for (const completion of deliveredCompletions) {
    state.wakeDeferredTaskIds.delete(completion.task_id);
  }
  state.pendingLongRunning = [];
  state.pendingPatternMatches = [];
  state.retryDelayMs = null;
  state.wakeRetryAttempts = 0;
  state.wakeHardStopped = false;
  // C-#1 site 4: tool-result piggyback delivery (the reminder is returned below
  // and cannot fail here), so mark ack-target ids awaiting-ack, then confirm/keep
  // on the ack result (keep → re-ack on a later forced drain, C-#3).
  const inTurnAckIds = completionAcks.map((completion) => completion.task_id);
  markDeliveredAwaitingAck(state, inTurnAckIds);
  const inTurnAcked = await ackCompletions(drainContext, completionAcks);
  if (inTurnAcked) confirmAcked(state, inTurnAckIds);
  // Cancel any pending debounced wake — its captured pendingCompletions /
  // pendingLongRunning are now drained, and firing the timer anyway would
  // build an empty-body "[BACKGROUND BASH STILL RUNNING]" reminder.
  clearWakeTimerIfNoPending(state);
  return [...content, { type: "text", text: reminder }];
}

export async function handleTurnEndBgCompletions(
  drainContext: DrainContext & { runtime: SendUserMessageRuntime },
): Promise<void> {
  stateFor(drainContext.sessionID).wakeDeferredTaskIds.clear();
  await triggerWakeIfPending(drainContext, false, true);
}

/**
 * Subc bg_events wake entrypoint (forced unconditional drain). The subc nudge is
 * payload-less. The module sends one optimistic nudge when the subscription opens,
 * then re-nudges while completions or pattern matches remain unacknowledged, so it
 * always means "drain me now" — even for a task
 * this process never tracked (prior session / already-cleared outstanding entry),
 * which the gated drain in {@link triggerWakeIfPending} would skip, leaving the
 * module to re-arm and nudge forever. The module-side loop lives at
 * `crates/aft/src/subc/push.rs::{emit_bg_event_wakes,clear_stale_bg_wakes_for_empty_sessions}`:
 * it re-emits pending wakes until ack clears durable items, so coalescing a duplicate
 * while this handler is in flight is safe. See the OpenCode twin for the full rationale.
 */
export function handleSubcBgEventsNudge(
  drainContext: DrainContext & { runtime: SendUserMessageRuntime },
): Promise<void> {
  const resolvedDrainContext: ResolvedDrainContext & { runtime: SendUserMessageRuntime } = {
    ...drainContext,
    sessionID: normalizeSessionID(drainContext.sessionID),
  };
  const key = `${resolvedDrainContext.directory}\u0000${resolvedDrainContext.sessionID}`;
  const inFlight = subcNudgesInFlight.get(key);
  if (inFlight) {
    logSubcNudgeLifecycle(
      resolvedDrainContext,
      "coalesced-in-flight",
      "nudge coalesced cause=handler-already-in-flight",
    );
    return inFlight;
  }

  logSubcNudgeLifecycle(resolvedDrainContext, "handler-entry", "nudge handler entered");
  const handling = handleSubcBgEventsNudgeOnce(resolvedDrainContext);
  subcNudgesInFlight.set(key, handling);
  const clear = (): void => {
    if (subcNudgesInFlight.get(key) === handling) subcNudgesInFlight.delete(key);
  };
  void handling.then(clear, clear);
  return handling;
}

async function handleSubcBgEventsNudgeOnce(
  drainContext: ResolvedDrainContext & { runtime: SendUserMessageRuntime },
): Promise<void> {
  // Resolve before touching session state. A stale nudge must be a terminal,
  // side-effect-free rejection rather than a reason to create a successor.
  bridgeForDrain(drainContext);
  const state = stateFor(drainContext.sessionID);
  state.wakeDeferredTaskIds.clear();
  rearmHardStoppedWake(state, drainContext);
  await triggerWakeIfPending(drainContext, false, true, true);
}

function logSubcNudgeLifecycle(
  drainContext: ResolvedDrainContext,
  kind: string,
  message: string,
): void {
  logDeliveryHop(drainContext, kind, message, {
    event: "subc_bg_nudge_delivery",
    cause: kind,
  });
}

function logDeliveryHop(
  drainContext: DrainContext,
  kind: string,
  message: string,
  data: Record<string, unknown>,
  level: "info" | "warn" = "info",
): void {
  const sessionID = normalizeSessionID(drainContext.sessionID);
  const taskKey =
    typeof data.task_id === "string"
      ? data.task_id
      : Array.isArray(data.task_ids)
        ? data.task_ids.join(",")
        : "";
  const key = `${kind}\u0000${taskKey}\u0000${drainContext.directory}\u0000${sessionID}`;
  const now = Date.now();
  const state = subcNudgeLogState.get(key);
  if (state && now - state.lastEmittedAt < SUBC_NUDGE_LOG_INTERVAL_MS) {
    state.suppressed += 1;
    return;
  }
  const suppressed = state?.suppressed ?? 0;
  subcNudgeLogState.set(key, { lastEmittedAt: now, suppressed: 0 });
  const payload = {
    ...data,
    canonical_root: drainContext.directory,
    suppressed,
  };
  if (level === "warn") {
    sessionWarn(sessionID, `${LOG_PREFIX} ${message}`, payload);
  } else {
    sessionLog(sessionID, `${LOG_PREFIX} ${message}`, payload);
  }
}

function logPerTaskDeliveryHop(
  drainContext: DrainContext,
  kind: string,
  message: string,
  taskIDs: readonly string[],
  data: Record<string, unknown>,
  level: "info" | "warn" = "info",
): void {
  for (const taskID of taskIDs) {
    logDeliveryHop(
      drainContext,
      kind,
      message,
      { ...data, task_id: taskID, task_ids: taskIDs },
      level,
    );
  }
}

function rearmHardStoppedWake(state: SessionBgState, drainContext: DrainContext): void {
  if (!state.wakeHardStopped) return;
  state.wakeHardStopped = false;
  state.wakeRetryAttempts = 0;
  state.retryDelayMs = null;
  logDeliveryHop(drainContext, "inject-rearmed", "delivery retry re-armed", {
    event: "bash_completion_inject_rearmed",
    task_ids: state.pendingCompletions.map((completion) => completion.task_id),
  });
}

async function withBgHopTimeout<T>(operation: Promise<T>, label: string): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      operation,
      new Promise<T>((_, reject) => {
        timer = setTimeout(
          () => reject(new Error(`${label} timed out after ${bgHopTimeoutMs}ms`)),
          bgHopTimeoutMs,
        );
        timer.unref?.();
      }),
    ]);
  } finally {
    if (timer) clearTimeout(timer);
  }
}

async function triggerWakeIfPending(
  drainContext: DrainContext & { runtime: SendUserMessageRuntime },
  skipDrain: boolean,
  includeDeferredCompletions = true,
  forceDrain = false,
): Promise<void> {
  // Note: previously bailed on `isActive()` (bridge.hasPendingRequests())
  // to defer wakes until the bridge was idle. That was wrong: the bridge
  // is busy for any non-agent traffic (status polls, configure work),
  // which orphaned completions when no other trigger fired. Pi's
  // `sendUserMessage` with `deliverAs: "steer"` handles ordinary mid-turn
  // delivery cleanly. For tasks spawned in the current assistant turn,
  // wakeDeferredTaskIds still suppresses immediate push wakes until an
  // in-turn append consumes the completion or turn end clears the deferral.
  // Mirrors the OpenCode fix.
  const state = stateFor(drainContext.sessionID);

  if (
    !skipDrain &&
    (forceDrain || state.outstandingTaskIds.size > 0 || !state.forcedDrainCompleted)
  ) {
    await drainCompletions(drainContext);
  }
  routeExplicitControlCompletions(state);
  if (!hasWakeEligiblePending(state, includeDeferredCompletions)) return;

  scheduleWake(
    state,
    async (reminder, deliveredCompletions) => {
      // Pi rejects sendUserMessage with "Agent is already processing" when
      // the agent is mid-turn unless we pass `deliverAs`. Use `steer`:
      // Pi delivers steering messages after the current tool batch finishes
      // and BEFORE the next LLM call (see agent-session.ts steer() docs:
      // "Delivered after the current assistant turn finishes executing its
      // tool calls, before the next LLM call"). That's exactly when we
      // want a background-bash completion to land — the agent sees the
      // result and can incorporate it into the very next thinking step
      // instead of writing a conclusion that didn't know the build/test
      // had finished.
      //
      // `followUp` would queue until the entire turn ends, which is too
      // late for tool-loop scenarios where the agent is actively working
      // on a problem that depends on the bash result.
      //
      // Unlike OpenCode, Pi's `sendUserMessage` does not accept any model
      // or variant fields — it just queues a content string. The next
      // turn uses Pi's currently-selected model, so there is no per-message
      // override for us to thread through.
      const ackTargetIds = deliveredCompletions.map((completion) => completion.task_id);
      logPerTaskDeliveryHop(drainContext, "inject-start", "session inject start", ackTargetIds, {
        event: "bash_completion_wake_send_user_message_start",
        attempt: state.wakeRetryAttempts + 1,
      });
      try {
        drainContext.runtime.sendUserMessage(reminder, { deliverAs: "steer" });
      } catch (err) {
        logPerTaskDeliveryHop(
          drainContext,
          "inject-error",
          "session inject failed",
          ackTargetIds,
          {
            event: "bash_completion_wake_send_user_message_error",
            attempt: state.wakeRetryAttempts + 1,
            cause: err instanceof Error ? err.message : String(err),
          },
          "warn",
        );
        throw err;
      }
      logPerTaskDeliveryHop(drainContext, "inject-ok", "session inject ok", ackTargetIds, {
        event: "bash_completion_wake_send_user_message_ok",
        attempt: state.wakeRetryAttempts + 1,
      });
      // Session injection completed synchronously. Only now may these task IDs
      // enter awaiting-ack state and become eligible for daemon acknowledgement.
      markDeliveredAwaitingAck(state, ackTargetIds);
      const acked = await ackCompletions(drainContext, deliveredCompletions);
      if (acked) confirmAcked(state, ackTargetIds);
    },
    (err, hardStopped) => {
      if (isTerminalNudgeError(err)) {
        sessionWarn(drainContext.sessionID ?? "", `${LOG_PREFIX} nudge rejected; terminal`, {
          event: "subc_bg_nudge_terminal",
          code: "root_generation_expired",
          canonical_root: drainContext.nudgeRef?.canonicalRoot,
          expected_generation: drainContext.nudgeRef?.generation,
          expected_concrete_pool_id: drainContext.nudgeRef?.concretePoolId,
        });
        return;
      }
      if (hardStopped) {
        logDeliveryHop(drainContext, "inject-hard-stopped", "delivery retries paused", {
          event: "bash_completion_inject_hard_stopped",
          cause: err instanceof Error ? err.message : String(err),
        });
      }
    },
    includeDeferredCompletions,
  );
}

export function formatSystemReminder(completions: readonly BgCompletion[]): string {
  const bullets = completions.map((completion) => formatCompletion(completion)).join("\n");
  // Only point at bash_status when at least one completion is truncated;
  // for fully-captured short outputs the agent already has the full result.
  const anyTruncated = completions.some((c) => c.output_truncated === true);
  const tail = anyTruncated
    ? `\n\nFor truncated tasks, use bash_status({ task_id: "..." }) to retrieve full output.`
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
  return `<system-reminder>\n[BACKGROUND BASH STILL RUNNING]\n${bullets}\nUse bash_status({ task_id: "..." }) to inspect output or bash_kill({ task_id: "..." }) to terminate.\n</system-reminder>`;
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

export function __setBgNotificationHopTimeoutForTests(timeoutMs: number): void {
  bgHopTimeoutMs = timeoutMs;
}

export function __resetBgNotificationStateForTests(): void {
  for (const state of sessionBgStates.values()) {
    if (state.debounceTimer) clearTimeout(state.debounceTimer);
  }
  sessionBgStates.clear();
  subcNudgesInFlight.clear();
  subcNudgeLogState.clear();
  bgHopTimeoutMs = DEFAULT_BG_HOP_TIMEOUT_MS;
}

function bridgeForDrain(drainContext: DrainContext): AftProjectTransport {
  if (drainContext.resolvedBridge) return drainContext.resolvedBridge;
  const bridge = drainContext.nudgeRef
    ? resolveBridgeForNudge(drainContext.ctx.pool, drainContext.nudgeRef)
    : drainContext.ctx.pool.getActiveBridgeForRoot(drainContext.directory);
  if (!bridge) {
    throw new Error(`active bridge unavailable for ${drainContext.directory}`);
  }
  drainContext.resolvedBridge = bridge;
  return bridge;
}

function isTerminalNudgeError(error: unknown): boolean {
  return (
    error instanceof Error &&
    (error as Error & { code?: unknown }).code === "root_generation_expired"
  );
}

async function drainCompletions(drainContext: DrainContext): Promise<void> {
  try {
    const bridge = bridgeForDrain(drainContext);
    const state = stateFor(drainContext.sessionID);
    const params = drainContext.sessionID ? { session_id: drainContext.sessionID } : {};
    const response = await withBgHopTimeout(
      bridge.send("bash_drain_completions", params, { timeoutMs: bgHopTimeoutMs }),
      "bash_drain_completions",
    );
    if (response.success === false) {
      logDeliveryHop(
        drainContext,
        "drain-error",
        "drain failed",
        {
          event: "bash_completion_drain_error",
          cause: String(response.message ?? "unknown error"),
        },
        "warn",
      );
      return;
    }
    state.forcedDrainCompleted = true;
    const drainedCompletions = Array.isArray(response.bg_completions)
      ? response.bg_completions.filter(isBgCompletion)
      : [];
    const drainedMatches = Array.isArray(response.pending_matches)
      ? response.pending_matches.filter(isPatternMatchEntry)
      : [];
    const drainedTaskIds = [
      ...new Set([
        ...drainedCompletions.map((completion) => completion.task_id),
        ...drainedMatches.map((match) => match.task_id),
      ]),
    ];
    logDeliveryHop(drainContext, "drain-ok", "drain ok", {
      event: "bash_completion_drain_ok",
      count: drainedTaskIds.length,
      task_ids: drainedTaskIds,
    });
    // C-#3 / R2-T3: reconcile awaiting-ack against the drain snapshot (daemon
    // truth). A task the drain still returns is held unacked → re-ack (stops the
    // nudges, self-terminating); one it no longer returns was dropped daemon-side
    // → reconcile forgets it. Ingest skips awaiting-ack tasks, so re-ack never
    // double-delivers.
    const reackTaskIds = reconcileAwaitingAck(state, drainedTaskIds);
    ingestDrainedBgCompletions(drainContext.sessionID, drainedCompletions);
    ingestDrainedPatternMatches(drainContext.sessionID, drainedMatches);
    if (reackTaskIds.length > 0) {
      const reacked = await ackCompletions(
        drainContext,
        reackTaskIds.map((task_id) => ({
          task_id,
          status: "unknown",
          exit_code: null,
          command: "",
        })),
      );
      if (reacked) confirmAcked(state, reackTaskIds);
    }
  } catch (err) {
    if (drainContext.nudgeRef && isTerminalNudgeError(err)) throw err;
    logDeliveryHop(
      drainContext,
      "drain-error",
      "drain failed",
      {
        event: "bash_completion_drain_error",
        cause: err instanceof Error ? err.message : String(err),
      },
      "warn",
    );
  }
}

async function ackCompletions(
  drainContext: DrainContext,
  completions: readonly BgCompletion[],
): Promise<boolean> {
  const taskIds = [...new Set(completions.map((completion) => completion.task_id))];
  if (taskIds.length === 0) return true;
  try {
    const bridge = bridgeForDrain(drainContext);
    const params = drainContext.sessionID
      ? { session_id: drainContext.sessionID, task_ids: taskIds }
      : { task_ids: taskIds };
    const response = await withBgHopTimeout(
      bridge.send("bash_ack_completions", params, { timeoutMs: bgHopTimeoutMs }),
      "bash_ack_completions",
    );
    if (response.success === false) {
      logPerTaskDeliveryHop(
        drainContext,
        "ack-error",
        "ack failed",
        taskIds,
        {
          event: "bash_completion_ack_error",
          cause: String(response.message ?? "unknown error"),
        },
        "warn",
      );
      return false;
    }
    logPerTaskDeliveryHop(drainContext, "ack-ok", "ack ok", taskIds, {
      event: "bash_completion_ack_ok",
    });
    return true;
  } catch (err) {
    if (drainContext.nudgeRef && isTerminalNudgeError(err)) throw err;
    logPerTaskDeliveryHop(
      drainContext,
      "ack-error",
      "ack failed",
      taskIds,
      {
        event: "bash_completion_ack_error",
        cause: err instanceof Error ? err.message : String(err),
      },
      "warn",
    );
    return false;
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
    state.pendingCompletions.length > 0 ||
    state.pendingLongRunning.length > 0 ||
    state.pendingPatternMatches.length > 0
  ) {
    return;
  }
  if (state.debounceTimer) clearTimeout(state.debounceTimer);
  state.debounceTimer = null;
  state.firstCompletionAt = null;
  state.scheduledFireAt = null;
  state.scheduledCompletionCount = 0;
  state.retryDelayMs = null;
  state.wakeRetryAttempts = 0;
  state.wakeHardStopped = false;
}

function scheduleWake(
  state: SessionBgState,
  sendWake: (reminder: string, completions: readonly BgCompletion[]) => Promise<void>,
  onSendFailure: (err: unknown, hardStopped: boolean) => void,
  includeDeferredCompletions = true,
): void {
  if (state.wakeHardStopped) return;
  // Race model: JS state changes are synchronous; awaits only happen before scheduling
  // drains and during final user-message delivery. Multiple hook invocations can
  // interleave only at those awaits, so we gate timer extension on completion count.
  const now = Date.now();
  const pendingCount =
    wakeEligibleCompletions(state, includeDeferredCompletions).length +
    state.pendingLongRunning.length +
    state.pendingPatternMatches.length;
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
  state.debounceTimer = setTimeout(() => {
    const pending = wakeEligibleCompletions(state, includeDeferredCompletions);
    const pendingLongRunning = state.pendingLongRunning;
    const pendingPatternMatches = state.pendingPatternMatches;
    state.debounceTimer = null;
    state.firstCompletionAt = null;
    state.scheduledFireAt = null;
    state.scheduledCompletionCount = 0;
    // Defensive: if another path (e.g. appendToolResultBgCompletions)
    // drained the pending arrays between schedule and fire and didn't
    // cancel us, just skip — don't ship an empty
    // "[BACKGROUND BASH STILL RUNNING]" shell.
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
    const deliveredTaskIds = new Set(pending.map((completion) => completion.task_id));
    state.pendingCompletions = state.pendingCompletions.filter(
      (completion) => !deliveredTaskIds.has(completion.task_id),
    );
    for (const taskId of deliveredTaskIds) state.wakeDeferredTaskIds.delete(taskId);
    state.pendingLongRunning = [];
    state.pendingPatternMatches = [];
    const completionAcks = completionAcksForDelivery(pending, pendingPatternMatches);
    // C-#1 site 1: mark ALL ack-target ids (completions + ack-on-delivery pattern
    // matches) as delivery-in-flight at the same sync tick they leave the queues,
    // before the sendWake await, so ingest skips them during delivery.
    const ackTargetIds = completionAcks.map((completion) => completion.task_id);
    markDelivering(state, ackTargetIds);
    void sendWake(reminder, completionAcks)
      .then(() => {
        state.retryDelayMs = null;
        state.wakeRetryAttempts = 0;
        state.wakeHardStopped = false;
      })
      .catch((err) => {
        // A stale nudge is terminal. Do not re-pend or retry it: the reference
        // has expired and must never revive a pool or mutate successor state.
        if (isTerminalNudgeError(err)) {
          clearDelivering(state, ackTargetIds);
          clearAwaitingAck(state, ackTargetIds);
          state.retryDelayMs = null;
          state.wakeHardStopped = true;
          onSendFailure(err, true);
          return;
        }
        // C-#1 site 3: delivery failed before departing (sendWake rejects only
        // before the awaiting-ack move). Clear in-flight so re-pended tasks are
        // redeliverable on retry.
        clearDelivering(state, ackTargetIds);
        state.pendingCompletions = [...pending, ...state.pendingCompletions];
        state.pendingLongRunning = [...pendingLongRunning, ...state.pendingLongRunning];
        state.pendingPatternMatches = [...pendingPatternMatches, ...state.pendingPatternMatches];
        state.wakeRetryAttempts += 1;
        if (state.wakeRetryAttempts >= MAX_WAKE_SEND_ATTEMPTS) {
          state.retryDelayMs = null;
          state.wakeHardStopped = true;
          onSendFailure(err, true);
          return;
        }
        state.retryDelayMs = Math.min((delay || DEBOUNCE_STEP_MS) * 2, DEBOUNCE_CAP_MS);
        onSendFailure(err, false);
        scheduleWake(state, sendWake, onSendFailure, includeDeferredCompletions);
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
      pendingPatternMatches: [],
      explicitControlTasks: new Set(),
      debounceTimer: null,
      firstCompletionAt: null,
      scheduledFireAt: null,
      scheduledCompletionCount: 0,
      retryDelayMs: null,
      wakeRetryAttempts: 0,
      wakeHardStopped: false,
      forcedDrainCompleted: false,
      unknownCompletions: [],
      wakeDeferredTaskIds: new Set(),
      consumedTaskIds: new Set(),
      consumedTaskOrder: [],
      deliveringTaskIds: new Set(),
      deliveredAwaitingAckTaskIds: new Set(),
      lastSeenAt: now,
    };
    sessionBgStates.set(key, state);
  } else {
    state.lastSeenAt = now;
  }
  return state;
}

function ingestDrainedPatternMatches(
  sessionID: string | undefined,
  matches: readonly PatternMatchEntry[],
): void {
  const state = stateFor(sessionID);
  for (const match of matches) {
    state.outstandingTaskIds.delete(match.task_id);
    if (isDeliveringOrAwaitingAck(state, match.task_id)) continue;
    queuePendingPatternMatch(state, { ...match, ackCompletionOnDelivery: true });
  }
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
      queuePendingPatternMatch(state, completionToExitPattern(completion, true));
      continue;
    }
    // Suppress completions for tasks already consumed inline by a
    // bash_status wait (same dedupe as ingestBgCompletions push path).
    if (state.consumedTaskIds.has(completion.task_id)) continue;
    // C-#1: a completion mid-delivery or delivered-but-not-yet-acked must NOT be
    // re-accepted as a fresh delivery. drainCompletions re-acks the awaiting-ack
    // ones (C-#3) so the module's re-nudge loop terminates.
    if (isDeliveringOrAwaitingAck(state, completion.task_id)) continue;
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

export function cleanupIdleSessionStates(now: number = Date.now()): void {
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

/**
 * True if `taskId` has a delivery in flight or delivered-awaiting-ack — it has
 * already left a deliverable queue and will be acked, so a fresh ingest (push or
 * forced drain) must NOT re-accept it (audit C-#1 double-deliver guard).
 */
function isDeliveringOrAwaitingAck(state: SessionBgState, taskId: string): boolean {
  return state.deliveringTaskIds.has(taskId) || state.deliveredAwaitingAckTaskIds.has(taskId);
}

/** Mark ack-target task ids as delivery-in-flight (at queue-removal, before await). */
function markDelivering(state: SessionBgState, taskIds: readonly string[]): void {
  for (const id of taskIds) state.deliveringTaskIds.add(id);
}

/** Delivery succeeded: move from in-flight to delivered-awaiting-ack (before ack). */
function markDeliveredAwaitingAck(
  state: SessionBgState,
  taskIds: readonly string[],
  sessionID?: string,
): void {
  for (const id of taskIds) {
    state.deliveringTaskIds.delete(id);
    state.deliveredAwaitingAckTaskIds.add(id);
  }
  capDeliveredAwaitingAck(state, sessionID);
}

/** Delivery failed before it departed: return ids to redeliverable (clear in-flight). */
function clearDelivering(state: SessionBgState, taskIds: readonly string[]): void {
  for (const id of taskIds) state.deliveringTaskIds.delete(id);
}

/** Ack confirmed (Rust dropped them): stop tracking — they will never re-nudge. */
function confirmAcked(state: SessionBgState, taskIds: readonly string[]): void {
  for (const id of taskIds) state.deliveredAwaitingAckTaskIds.delete(id);
}

/** Terminal nudge rejection: discard local delivery markers without retrying. */
function clearAwaitingAck(state: SessionBgState, taskIds: readonly string[]): void {
  for (const id of taskIds) state.deliveredAwaitingAckTaskIds.delete(id);
}

/**
 * Reconcile the awaiting-ack set against a forced drain's snapshot (R2-T3, daemon
 * is source of truth). A task the drain STILL returns is held unacked → RE-ACK
 * (C-#3 close); a task the drain does NOT return was already dropped daemon-side
 * → forget. Returns ids to re-ack; the caller acks then confirms. Replaces a
 * timer eviction that could drop a still-held task and reopen the double-deliver.
 */
function reconcileAwaitingAck(state: SessionBgState, drainedTaskIds: readonly string[]): string[] {
  if (state.deliveredAwaitingAckTaskIds.size === 0) return [];
  const drainedIds = new Set(drainedTaskIds);
  const reack: string[] = [];
  for (const id of [...state.deliveredAwaitingAckTaskIds]) {
    if (drainedIds.has(id)) reack.push(id);
    else state.deliveredAwaitingAckTaskIds.delete(id); // daemon no longer holds it
  }
  return reack;
}

/** Pure OOM backstop: reconciliation is the real bound, so this should never fire. */
function capDeliveredAwaitingAck(state: SessionBgState, sessionID?: string): void {
  while (state.deliveredAwaitingAckTaskIds.size > DELIVERED_AWAITING_ACK_CAP) {
    const oldest = state.deliveredAwaitingAckTaskIds.values().next().value;
    if (oldest === undefined) break;
    state.deliveredAwaitingAckTaskIds.delete(oldest);
    sessionWarn(
      sessionID ?? "",
      `${LOG_PREFIX} deliveredAwaitingAckTaskIds exceeded ${DELIVERED_AWAITING_ACK_CAP}; evicting ${oldest}`,
    );
  }
}

function isPatternMatchEntry(value: unknown): value is PatternMatchEntry {
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  const match = value as Record<string, unknown>;
  return (
    typeof match.task_id === "string" &&
    typeof match.session_id === "string" &&
    typeof match.watch_id === "string" &&
    typeof match.match_text === "string" &&
    typeof match.match_offset === "number" &&
    typeof match.context === "string" &&
    typeof match.once === "boolean"
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
  // contain colors that bloat the reminder.
  // biome-ignore lint/suspicious/noControlCharactersInRegex: ANSI escape stripping requires \x1b
  const ansiRegex = /\x1b\[[0-9;]*[a-zA-Z]/g;
  const raw = (completion.output_preview ?? "").replace(ansiRegex, "");
  if (!raw.trim()) return "";
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
