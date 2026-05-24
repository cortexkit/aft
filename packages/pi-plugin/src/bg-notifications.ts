import { sessionWarn } from "./logger.js";
import type { PluginContext } from "./types.js";

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

interface DrainContext {
  ctx: PluginContext;
  directory: string;
  sessionID?: string;
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
  drainContext: DrainContext & { runtime: SendUserMessageRuntime },
  completion: unknown,
): Promise<void> {
  ingestBgCompletions(drainContext.sessionID, [completion]);
  await triggerWakeIfPending(drainContext, true);
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
    state.pendingLongRunning.length === 0
  )
    await drainCompletions(drainContext);
  if (
    state.outstandingTaskIds.size === 0 &&
    state.pendingCompletions.length === 0 &&
    state.pendingLongRunning.length === 0
  )
    return undefined;

  if (state.outstandingTaskIds.size > 0 || !state.forcedDrainCompleted) {
    await drainCompletions(drainContext);
  }
  if (state.pendingCompletions.length === 0 && state.pendingLongRunning.length === 0)
    return undefined;

  const deliveredCompletions = [...state.pendingCompletions];
  const reminder = formatCombinedSystemReminder(state.pendingCompletions, state.pendingLongRunning);
  state.pendingCompletions = [];
  state.pendingLongRunning = [];
  state.wakeRetryAttempts = 0;
  state.wakeHardStopped = false;
  await ackCompletions(drainContext, deliveredCompletions);
  // Cancel any pending debounced wake — its captured pendingCompletions /
  // pendingLongRunning are now drained, and firing the timer anyway would
  // build an empty-body "[BACKGROUND BASH STILL RUNNING]" reminder.
  if (state.debounceTimer) {
    clearTimeout(state.debounceTimer);
    state.debounceTimer = null;
    state.firstCompletionAt = null;
    state.scheduledFireAt = null;
    state.scheduledCompletionCount = 0;
  }
  return [...content, { type: "text", text: reminder }];
}

export async function handleTurnEndBgCompletions(
  drainContext: DrainContext & { runtime: SendUserMessageRuntime },
): Promise<void> {
  await triggerWakeIfPending(drainContext, false);
}

async function triggerWakeIfPending(
  drainContext: DrainContext & { runtime: SendUserMessageRuntime },
  skipDrain: boolean,
): Promise<void> {
  // Note: previously bailed on `isActive()` (bridge.hasPendingRequests())
  // to defer wakes until the bridge was idle. That was wrong: the bridge
  // is busy for any non-agent traffic (status polls, configure work),
  // which orphaned completions when no other trigger fired. Pi's
  // `sendUserMessage` with `deliverAs: "steer"` already handles mid-turn
  // delivery cleanly, so suppressing the wake provides no benefit and
  // creates a hang. Mirrors the OpenCode fix.
  const state = stateFor(drainContext.sessionID);

  if (!skipDrain && (state.outstandingTaskIds.size > 0 || !state.forcedDrainCompleted)) {
    await drainCompletions(drainContext);
  }
  if (state.pendingCompletions.length === 0 && state.pendingLongRunning.length === 0) return;

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
      drainContext.runtime.sendUserMessage(reminder, { deliverAs: "steer" });
      await ackCompletions(drainContext, deliveredCompletions);
    },
    (err, hardStopped) => {
      sessionWarn(
        drainContext.sessionID ?? "",
        hardStopped
          ? `${LOG_PREFIX} wake send failed ${MAX_WAKE_SEND_ATTEMPTS} times; stopping retries: ${err instanceof Error ? err.message : String(err)}`
          : `${LOG_PREFIX} wake send failed: ${err instanceof Error ? err.message : String(err)}`,
      );
    },
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

function formatCombinedSystemReminder(
  completions: readonly BgCompletion[],
  longRunning: readonly BgLongRunningReminder[],
): string {
  if (completions.length === 0) return formatLongRunningReminder(longRunning);
  if (longRunning.length === 0) return formatSystemReminder(completions);
  return `${formatSystemReminder(completions)}\n${formatLongRunningReminder(longRunning)}`;
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
    const params = sessionID ? { session_id: sessionID } : {};
    const response = await bridge.send("bash_drain_completions", params);
    if (response.success === false) {
      sessionWarn(
        sessionID ?? "",
        `${LOG_PREFIX} drain failed: ${String(response.message ?? "unknown error")}`,
      );
      return;
    }
    state.forcedDrainCompleted = true;
    ingestDrainedBgCompletions(sessionID, response.bg_completions);
  } catch (err) {
    sessionWarn(
      sessionID ?? "",
      `${LOG_PREFIX} drain failed: ${err instanceof Error ? err.message : String(err)}`,
    );
  }
}

async function ackCompletions(
  { ctx, directory, sessionID }: DrainContext,
  completions: readonly BgCompletion[],
): Promise<void> {
  const taskIds = [...new Set(completions.map((completion) => completion.task_id))];
  if (taskIds.length === 0) return;
  try {
    const bridge = ctx.pool.getActiveBridgeForRoot(directory) ?? ctx.pool.getBridge(directory);
    const params = sessionID ? { session_id: sessionID, task_ids: taskIds } : { task_ids: taskIds };
    const response = await bridge.send("bash_ack_completions", params);
    if (response.success === false) {
      sessionWarn(
        sessionID ?? "",
        `${LOG_PREFIX} ack failed: ${String(response.message ?? "unknown error")}`,
      );
    }
  } catch (err) {
    sessionWarn(
      sessionID ?? "",
      `${LOG_PREFIX} ack failed: ${err instanceof Error ? err.message : String(err)}`,
    );
  }
}

function scheduleWake(
  state: SessionBgState,
  sendWake: (reminder: string, completions: readonly BgCompletion[]) => Promise<void>,
  onSendFailure: (err: unknown, hardStopped: boolean) => void,
): void {
  if (state.wakeHardStopped) return;
  // Race model: JS state changes are synchronous; awaits only happen before scheduling
  // drains and during final user-message delivery. Multiple hook invocations can
  // interleave only at those awaits, so we gate timer extension on completion count.
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
  state.debounceTimer = setTimeout(() => {
    const pending = state.pendingCompletions;
    const pendingLongRunning = state.pendingLongRunning;
    state.debounceTimer = null;
    state.firstCompletionAt = null;
    state.scheduledFireAt = null;
    state.scheduledCompletionCount = 0;
    // Defensive: if another path (e.g. appendToolResultBgCompletions)
    // drained the pending arrays between schedule and fire and didn't
    // cancel us, just skip — don't ship an empty
    // "[BACKGROUND BASH STILL RUNNING]" shell.
    if (pending.length === 0 && pendingLongRunning.length === 0) return;
    const reminder = formatCombinedSystemReminder(pending, pendingLongRunning);
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
        scheduleWake(state, sendWake, onSendFailure);
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
