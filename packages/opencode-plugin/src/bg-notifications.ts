import { createHash, randomUUID } from "node:crypto";
import {
  type AftProjectTransport,
  type BgNudgeRef,
  resolveBridgeForNudge,
} from "@cortexkit/aft-bridge";
import { sessionLog, sessionWarn } from "./logger.js";
import { resolvePromptContext } from "./shared/last-assistant-model.js";
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
   * Task IDs spawned since the last session.idle event. Push completions for
   * these tasks are kept pending but do not promptAsync-wake immediately: the
   * agent may still be in the same assistant turn and about to call sync
   * bash_watch, whose inline result should be the only delivery. In-turn
   * append and the next session.idle still deliver normally.
   */
  wakeDeferredTaskIds: Set<string>;
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
  /**
   * Task IDs whose delivery is IN FLIGHT (removed from pendingCompletions /
   * pattern queues, prompt not yet resolved). Ingest skips these so a subc forced
   * drain in the removal→delivery window cannot re-accept and double-schedule an
   * already-departing completion. Bounded by the in-flight wake batch (cleared on
   * delivery success → moves to awaitingAck, or on delivery failure → re-pended).
   */
  deliveringTaskIds: Set<string>;
  /**
   * Task IDs DELIVERED to the agent but whose `bash_ack_completions` has not yet
   * confirmed (so the Rust registry still holds them and, over subc, re-nudges).
   * Ingest skips these for fresh delivery AND a forced drain RE-ACKs them (the
   * self-terminating close of the re-nudge loop, C-#3). Entries are removed by
    * DAEMON RECONCILIATION, not a timer: when a forced drain no longer returns a
    * task as unacknowledged, it is safe to forget (R2-T3 — a
   * time-based TTL could evict a task the daemon still holds, reopening the
   * double-deliver). Self-drains: the module re-nudges (→ drain → re-ack) until
   * ack confirms. Insertion-ordered for the FIFO OOM backstop cap.
   */
  deliveredAwaitingAckTaskIds: Set<string>;
  lastSeenAt: number;
};

const CONSUMED_TASKIDS_CAP = 256;
/**
 * Pure OOM backstop for deliveredAwaitingAckTaskIds (drain reconciliation is the
 * real bound — this should never realistically fire). On overflow the oldest is
 * evicted with a warn; evicting an entry the daemon still holds can reopen the
 * double-deliver window, so the cap is set far above any plausible in-flight set.
 */
const DELIVERED_AWAITING_ACK_CAP = 4096;

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
const SUBC_NUDGE_LOG_INTERVAL_MS = 60_000;
const DEFAULT_BG_HOP_TIMEOUT_MS = 15_000;
let bgHopTimeoutMs = DEFAULT_BG_HOP_TIMEOUT_MS;
const subcNudgesInFlight = new Map<string, Promise<void>>();
const subcNudgeLogState = new Map<string, { lastEmittedAt: number; suppressed: number }>();

interface DrainContext {
  ctx: PluginContext;
  directory: string;
  sessionID: string;
  /**
   * Plugin-provided OpenCode SDK client (`input.client`). Wake prompts are
   * sent through this canonical in-process client; OpenCode fixed the
   * runner-state split that previously required a live HTTP listener workaround.
   *
   * Typed `unknown` because the real `@opencode-ai/sdk` `OpencodeClient`
   * has a generated `promptAsync` signature. The wake closure asserts to the
   * loose structural `OpenCodeClient` shape after checking it.
   */
  client?: unknown;
  /** Complete provenance for a subc bg_events nudge. */
  nudgeRef?: BgNudgeRef;
  /** Cached bridge so one nudge resolves drain and ack through one authority. */
  resolvedBridge?: AftProjectTransport;
}

interface OpenCodeClient {
  session?: {
    promptAsync?: (input: unknown) => Promise<unknown> | unknown;
    messages?: (input: { path: { id: string } }) => Promise<{ data?: unknown[] }>;
  };
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
  state.wakeDeferredTaskIds.delete(taskId);
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
  // C-#1 site 5: an inline bash_watch wait already delivered this task's result
  // in its tool output. Mark it awaiting-ack so a forced drain racing this ack
  // re-acks rather than re-delivering, then confirm/keep on the ack result. (Do
  // NOT route this through consumedTaskIds — that set is also used pre-delivery
  // by markTaskWaiting, so re-acking all of it would be wrong.)
  const state = stateFor(drainContext.sessionID);
  markDeliveredAwaitingAck(state, [taskId]);
  const acked = await ackCompletions(drainContext, [
    { task_id: taskId, status: "unknown", exit_code: null, command: "" },
  ]);
  if (acked) confirmAcked(state, [taskId]);
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
  drainContext: DrainContext & { client: unknown },
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
  drainContext: DrainContext & { client: unknown },
  completion: unknown,
): Promise<void> {
  ingestBgCompletions(drainContext.sessionID, [completion]);
  await triggerWakeIfPending(drainContext, true, false);
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
  for (const completion of deliveredCompletions) {
    state.wakeDeferredTaskIds.delete(completion.task_id);
  }
  state.pendingLongRunning = [];
  state.pendingPatternMatches = [];
  state.retryDelayMs = null;
  state.wakeRetryAttempts = 0;
  state.wakeHardStopped = false;
  // C-#1 site 4: this path delivers via the tool-result piggyback (already
  // written to output.output above — delivery cannot fail here), so mark the
  // ack-target ids awaiting-ack directly, then confirm/keep on the ack result.
  const inTurnAckIds = completionAcks.map((completion) => completion.task_id);
  markDeliveredAwaitingAck(state, inTurnAckIds);
  const inTurnAcked = await ackCompletions(drainContext, completionAcks);
  if (inTurnAcked) confirmAcked(state, inTurnAckIds);
  // Cancel any pending debounced wake — its captured pendingCompletions /
  // pendingLongRunning are now drained, and firing the timer anyway would
  // build an empty-body system-reminder ("[BACKGROUND BASH STILL RUNNING]"
  // with no bullets) since the timer reads `state.pendingLongRunning`
  // again at fire time.
  clearWakeTimerIfNoPending(state);
}

export async function handleIdleBgCompletions(
  drainContext: DrainContext & { client: unknown },
): Promise<void> {
  stateFor(drainContext.sessionID).wakeDeferredTaskIds.clear();
  await triggerWakeIfPending(drainContext, false, true);
}

/**
 * Subc bg_events wake entrypoint. Over subc, an idle-completion WAKE is a thin
 * payload-less nudge. The module sends one optimistic nudge when the subscription
 * opens, then re-nudges while completions or pattern matches remain unacknowledged;
 * either case means "drain me now". This
 * differs from {@link handleIdleBgCompletions}, whose drain is GATED — once
 * `forcedDrainCompleted` is set and nothing is locally outstanding, it skips the
 * drain. A subc completion can be for a task this process never tracked (a prior
 * session, or one whose outstanding entry was already cleared), so the gated
 * drain would skip it and the module would re-arm and nudge forever. The
 * module-side loop is
 * `crates/aft/src/subc/push.rs::{emit_bg_event_wakes,clear_stale_bg_wakes_for_empty_sessions}`:
 * it re-emits pending wakes until ack clears durable items, so coalescing a duplicate
 * while this handler is in flight is safe. This path forces an UNCONDITIONAL drain
 * so each durable notification is fetched, delivered, and acked.
 */
export function handleSubcBgEventsNudge(
  drainContext: DrainContext & { client: unknown },
): Promise<void> {
  const key = `${drainContext.directory}\u0000${drainContext.sessionID}`;
  const inFlight = subcNudgesInFlight.get(key);
  if (inFlight) {
    logSubcNudgeLifecycle(
      drainContext,
      "coalesced-in-flight",
      "nudge coalesced cause=handler-already-in-flight",
    );
    return inFlight;
  }

  logSubcNudgeLifecycle(drainContext, "handler-entry", "nudge handler entered");
  const handling = handleSubcBgEventsNudgeOnce(drainContext);
  subcNudgesInFlight.set(key, handling);
  const clear = (): void => {
    if (subcNudgesInFlight.get(key) === handling) subcNudgesInFlight.delete(key);
  };
  void handling.then(clear, clear);
  return handling;
}

async function handleSubcBgEventsNudgeOnce(
  drainContext: DrainContext & { client: unknown },
): Promise<void> {
  // Resolve before touching session state. A stale nudge must be a terminal,
  // side-effect-free rejection rather than a reason to create a successor.
  bridgeForDrain(drainContext);
  const state = stateFor(drainContext.sessionID);
  state.wakeDeferredTaskIds.clear();
  rearmHardStoppedWake(state, drainContext);
  await triggerWakeIfPending(drainContext, false, true, true);
}

function logSubcNudgeLifecycle(drainContext: DrainContext, kind: string, message: string): void {
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
  const taskKey =
    typeof data.task_id === "string"
      ? data.task_id
      : Array.isArray(data.task_ids)
        ? data.task_ids.join(",")
        : "";
  const key = `${kind}\u0000${taskKey}\u0000${drainContext.directory}\u0000${drainContext.sessionID}`;
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
    sessionWarn(drainContext.sessionID, `${LOG_PREFIX} ${message}`, payload);
  } else {
    sessionLog(drainContext.sessionID, `${LOG_PREFIX} ${message}`, payload);
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

function promptAsyncFailure(response: unknown): string | null {
  if (!response || typeof response !== "object" || Array.isArray(response)) return null;
  const record = response as Record<string, unknown>;
  if (record.error != null) {
    return typeof record.error === "string" ? record.error : JSON.stringify(record.error);
  }
  if (record.success === false)
    return String(record.message ?? "promptAsync returned success=false");
  return null;
}

async function triggerWakeIfPending(
  drainContext: DrainContext & { client: unknown },
  skipDrain: boolean,
  includeDeferredCompletions = true,
  forceDrain = false,
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
      const taskIDs = deliveredCompletions.map((completion) => completion.task_id);

      const getInProcessClient = (): OpenCodeClient => {
        if (!drainContext.client) {
          sessionWarn(drainContext.sessionID, `${LOG_PREFIX} wake client unavailable`, {
            event: "bash_completion_wake_client_unavailable",
            task_ids: taskIDs,
            directory: drainContext.directory,
            attempt: state.wakeRetryAttempts + 1,
          });
          throw new Error("in-process wake client unavailable: input.client absent");
        }
        // Cast the unknown `input.client` (real SDK shape with a generated
        // narrower promptAsync signature) to the loose structural shape
        // the wake closure uses. The runtime check in `sendPrompt` confirms
        // shape before use.
        return drainContext.client as OpenCodeClient;
      };

      const sendPrompt = async (client: OpenCodeClient): Promise<string> => {
        const promptAsync = client.session?.promptAsync;
        if (typeof promptAsync !== "function") {
          throw new Error("wake client.session.promptAsync is unavailable");
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
          // `synthetic: true` (NOT `ignored`): this is an AGENT-DIRECTED wake —
          // the model should resume and act on the finished background task.
          // synthetic keeps it model-visible (OpenCode's toModelMessagesEffect
          // serializes parts on `!ignored && text!==""`, and createUserMessage →
          // loop still wakes the idle run) while:
          //  1. dropping it out of the TUI user-message render (it's a steer,
          //     not a real user turn), and
          //  2. exempting it from OpenCode's prompt.ts mid-turn `<system-reminder>`
          //     wrapper, whose `id > lastFinished.id` condition flips wrapped→
          //     unwrapped as lastFinished advances — re-serializing the same
          //     mid-tail message differently across passes and busting the
          //     prefix cache once per injection (anomalyco/opencode#129). A
          //     synthetic part is skipped by that wrapper, so it stays
          //     byte-stable and never busts the cache.
          // NEVER pair synthetic with `ignored` — `ignored` strips it from the
          // model call entirely, so the wake would not reach the agent.
          parts: [{ type: "text", text: reminder, synthetic: true }],
        };
        if (promptContext?.agent) body.agent = promptContext.agent;
        if (promptContext?.model) {
          body.model = {
            providerID: promptContext.model.providerID,
            modelID: promptContext.model.modelID,
          };
        }
        if (promptContext?.variant) body.variant = promptContext.variant;

        const attemptPrompt = async (
          attemptBody: Record<string, unknown>,
          mode: "mirrored-context" | "model-free-fallback",
        ): Promise<string> => {
          const deliveryID = `aftdel_${randomUUID()}`;
          const wakeMeta = {
            delivery_id: deliveryID,
            attempt: state.wakeRetryAttempts + 1,
            task_ids: taskIDs,
            directory: drainContext.directory,
            reminder_sha256: hashReminder(reminder),
            reminder_chars: reminder.length,
            injection_mode: mode,
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
          logPerTaskDeliveryHop(
            drainContext,
            `inject-start-${mode}`,
            "session inject start",
            taskIDs,
            { event: "bash_completion_wake_prompt_async_start", ...wakeMeta },
          );
          try {
            const response = await withBgHopTimeout(
              Promise.resolve(
                promptAsync({
                  path: { id: drainContext.sessionID },
                  body: attemptBody,
                }),
              ),
              "session promptAsync injection",
            );
            const failure = promptAsyncFailure(response);
            if (failure) throw new Error(failure);
          } catch (err) {
            logPerTaskDeliveryHop(
              drainContext,
              "inject-error",
              "session inject failed",
              taskIDs,
              {
                event: "bash_completion_wake_prompt_async_error",
                delivery_id: deliveryID,
                attempt: state.wakeRetryAttempts + 1,
                injection_mode: mode,
                cause: err instanceof Error ? err.message : String(err),
              },
              "warn",
            );
            throw err;
          }
          logPerTaskDeliveryHop(drainContext, `inject-ok-${mode}`, "session inject ok", taskIDs, {
            event: "bash_completion_wake_prompt_async_ok",
            delivery_id: deliveryID,
            attempt: state.wakeRetryAttempts + 1,
            injection_mode: mode,
          });
          return deliveryID;
        };

        try {
          return await attemptPrompt(body, "mirrored-context");
        } catch (err) {
          if (!promptContext?.model) throw err;
          const fallbackBody = { ...body };
          delete fallbackBody.model;
          delete fallbackBody.variant;
          return attemptPrompt(fallbackBody, "model-free-fallback");
        }
      };

      let deliveryID: string;
      try {
        deliveryID = await sendPrompt(getInProcessClient());
      } catch (err) {
        logPerTaskDeliveryHop(
          drainContext,
          "inject-error",
          "session inject failed",
          taskIDs,
          {
            event: "bash_completion_wake_prompt_async_error",
            cause: err instanceof Error ? err.message : String(err),
          },
          "warn",
        );
        throw err;
      }
      // Session injection has resolved successfully. Only now may these task IDs
      // enter awaiting-ack state and become eligible for daemon acknowledgement.
      markDeliveredAwaitingAck(state, taskIDs);
      const acked = await ackCompletions(drainContext, deliveredCompletions, deliveryID);
      if (acked) confirmAcked(state, taskIDs);
    },
    (err, hardStopped) => {
      if (isTerminalNudgeError(err)) {
        sessionWarn(drainContext.sessionID, `${LOG_PREFIX} nudge rejected; terminal`, {
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
    drainContext.sessionID,
    includeDeferredCompletions,
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
    const response = await withBgHopTimeout(
      bridge.send(
        "bash_drain_completions",
        { session_id: drainContext.sessionID },
        { timeoutMs: bgHopTimeoutMs },
      ),
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
    // C-#3 / R2-T3: reconcile the awaiting-ack set against the drain snapshot
    // (daemon truth). A task the drain still returns is held unacked → re-ack it
    // so the registry drops it and the nudges stop (self-terminating). A task the
    // drain no longer returns was already dropped daemon-side → reconcile forgets
    // it. ingestDrainedBgCompletions skips awaiting-ack tasks for fresh delivery,
    // so a re-ack never double-delivers.
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
  deliveryID?: string,
): Promise<boolean> {
  const taskIds = [...new Set(completions.map((completion) => completion.task_id))];
  if (taskIds.length === 0) return true;
  try {
    const bridge = bridgeForDrain(drainContext);
    const response = await withBgHopTimeout(
      bridge.send(
        "bash_ack_completions",
        { session_id: drainContext.sessionID, task_ids: taskIds },
        { timeoutMs: bgHopTimeoutMs },
      ),
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
    // Trace #6 of 7: bash_ack_completions succeeded on the Rust side.
    // Closes the wake chain: scheduled → fire → start → ok → ack_ok.
    // Note: ack also runs from appendInTurnBgCompletions without a
    // deliveryID — that path uses trace #7 (in_turn_append) instead, so
    // ack_ok carries delivery_id only when present.
    logPerTaskDeliveryHop(drainContext, "ack-ok", "ack ok", taskIds, {
      event: "bash_completion_ack_ok",
      delivery_id: deliveryID ?? null,
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
  sessionID?: string,
  includeDeferredCompletions = true,
): void {
  if (state.wakeHardStopped) return;
  // Race model: JS state changes are synchronous; awaits only happen before scheduling
  // drains and during final prompt delivery. Multiple hook invocations can interleave
  // only at those awaits, so we gate timer extension on the pending completion count.
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

    const deliveredTaskIds = new Set(pending.map((completion) => completion.task_id));
    state.pendingCompletions = state.pendingCompletions.filter(
      (completion) => !deliveredTaskIds.has(completion.task_id),
    );
    for (const taskId of deliveredTaskIds) state.wakeDeferredTaskIds.delete(taskId);
    state.pendingLongRunning = [];
    state.pendingPatternMatches = [];
    const completionAcks = completionAcksForDelivery(pending, pendingPatternMatches);
    // C-#1 site 1: mark ALL ack-target ids (completions + ack-on-delivery pattern
    // matches, not just `pending`) as delivery-in-flight at the same synchronous
    // tick they leave the queues, BEFORE the sendWake await. Ingest skips them so
    // a forced drain in the delivery window can't re-accept them.
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
        // C-#1 site 3: delivery failed BEFORE departing (sendWake only rejects on
        // getInProcessClient/sendPrompt throw, which is before the awaiting-ack
        // move — ackCompletions never throws). Clear the in-flight marks so the
        // re-prepended tasks are redeliverable on the retry.
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
        scheduleWake(state, sendWake, onSendFailure, sessionID, includeDeferredCompletions);
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
    // C-#1: a completion mid-delivery (in flight) or delivered-but-not-yet-acked
    // must NOT be re-accepted as a fresh delivery, or a forced drain in that
    // window double-delivers it. The drainCompletions caller separately re-acks
    // the awaiting-ack ones (C-#3) so the module's re-nudge loop terminates.
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
 * True if `taskId` has a delivery in flight or delivered-awaiting-ack — i.e. it
 * has already left a deliverable queue and will be acked, so a fresh ingest
 * (push or forced drain) must NOT re-accept it (audit C-#1 double-deliver guard).
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
 * Reconcile the awaiting-ack set against a forced drain's snapshot (R2-T3, the
 * daemon is the source of truth). For each awaiting-ack task: if the drain
 * STILL returns it, the daemon holds it unacked → RE-ACK it (C-#3 close). If the
 * drain does NOT return it, the daemon already dropped it (a prior ack landed or
 * it was consumed elsewhere) → forget it. Returns the ids to re-ack; the caller
 * acks then confirms. This replaces a timer-based eviction that could drop a
 * still-held task and reopen the double-deliver window.
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

/**
 * Pure OOM backstop: drain reconciliation is the real bound, so this should never
 * fire. If it does, evict the oldest (insertion order) with a warn — accepting
 * that evicting a still-held task can reopen the double-deliver window, which is
 * the lesser evil vs unbounded growth.
 */
function capDeliveredAwaitingAck(state: SessionBgState, sessionID?: string): void {
  while (state.deliveredAwaitingAckTaskIds.size > DELIVERED_AWAITING_ACK_CAP) {
    const oldest = state.deliveredAwaitingAckTaskIds.values().next().value;
    if (oldest === undefined) break;
    state.deliveredAwaitingAckTaskIds.delete(oldest);
    sessionWarn(
      sessionID,
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
