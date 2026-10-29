import type {
  AftProjectTransport,
  AftTransportFactoryOptions,
  AftTransportPool,
  BashCompletedPayload,
  BashLongRunningPayload,
  BashPatternMatchAck,
  BashWatchLineBatch,
  BgNudgeRef,
  BridgeOptions,
} from "@cortexkit/aft-bridge";
import {
  canonicalizeProjectRoot,
  WatchBatchProgress,
  watchBatchHasContent,
} from "@cortexkit/aft-bridge";
import { Effect } from "effect";

import { interruptBashWaitsForChatMessage } from "../bash-wait-detach.js";
import {
  consumeBgCompletion,
  formatLongRunningReminder,
  formatPatternMatchReminder,
  formatSystemReminder,
  handleSubcBgEventsNudge,
  holdBgCompletion,
  isPatternMatchEntry,
  markBgCompletionDelivered,
  type PatternMatchEntry,
} from "../bg-notifications.js";
import type { AftConfig } from "../config.js";
import { warn } from "../logger.js";
import { releaseSessionState } from "../shared/session-cleanup.js";
import { resolveIsSubagent } from "../shared/subagent-detect.js";
import { executeV2Bash } from "../tools/bash/executor.js";
import type { V2ToolConsumers } from "../tools/definitions/v2.js";
import {
  awaitV2ExecutionSettlement,
  forgetV2Watch,
  noteV2ToolAbort,
  v2SessionDeleted,
  v2SubagentStopped,
  v2WatchWakeAllowed,
} from "../v2-session-lifecycle.js";
import { type V2SessionAdmission, V2SessionDelivery } from "./session-delivery.js";

export interface V2WakeHostContext {
  readonly session: V2SessionAdmission;
}

/**
 * How one Location delivers bridge notifications for its sessions.
 *
 * Routes are stored process-wide and may be called by a different copy of this
 * module (see `wakeRouteState`), possibly one of a different plugin version
 * with its own `effect` instance. So a route exposes only plain
 * Promise-returning functions built by the copy that owns the session, and the
 * shape must stay backward compatible for as long as the state key is unchanged.
 */
interface WakeRoute {
  /** Deliver one completion to its session and acknowledge it on `bridge` once admitted. */
  completion(
    completion: BashCompletedPayload,
    bridge: AftProjectTransport,
    acknowledge?: boolean,
  ): Promise<boolean | undefined>;
  standaloneCompletion?(
    completion: BashCompletedPayload,
    bridge: AftProjectTransport,
  ): Promise<void>;
  patternMatch?(match: PatternMatchEntry, bridge: AftProjectTransport): Promise<boolean>;
  /** Record a status-only long-running reminder in its session. */
  longRunning(reminder: BashLongRunningPayload): Promise<void>;
  /** Optional for route registrations created before pattern-match delivery was added. */
  standalonePatternMatch?(match: PatternMatchEntry, bridge: AftProjectTransport): Promise<void>;
  drainPatterns?(sessionID: string, root?: string): Promise<void>;
}

/** A completion that arrived while no Location had its session registered. */
interface UnroutedCompletion {
  readonly completion: BashCompletedPayload;
  /** The bridge that pushed it, which is where the acknowledgement must go. */
  readonly bridge: AftProjectTransport;
}

interface WakeRouteState {
  /** Session id -> the route of the Location that last registered it. */
  readonly routes: Map<string, WakeRoute>;
  /** Session id -> task id -> completion waiting for its session to register. */
  readonly unrouted: Map<string, Map<string, UnroutedCompletion>>;
  /** Sessions already warned about since they were last routed, to log once per session. */
  readonly warnedUnrouted: Set<string>;
}

/**
 * The standalone bridge pool is one per process (see `location-lifecycle.ts`
 * in aft-bridge) and keeps the notification callbacks of the first Location
 * that created it. OpenCode 2 builds a runtime per Location and can load a
 * separate copy of this module for each (seen with a local-source plugin; an
 * npm plugin gets a second copy when a newly installed version is loaded from
 * its own directory while the old one still runs). A module-level table would then hold only the
 * sessions of the first Location, and completions for every other Location's
 * sessions would be dropped. Keeping the table on `globalThis` under a
 * `Symbol.for` key makes every copy share it.
 */
const WAKE_ROUTE_STATE: unique symbol = Symbol.for(
  "@cortexkit/aft-opencode/wake-routes/v1",
) as never;

type WakeRouteGlobal = typeof globalThis & {
  [WAKE_ROUTE_STATE]?: WakeRouteState;
};

function wakeRouteState(): WakeRouteState {
  const host = globalThis as WakeRouteGlobal;
  return (host[WAKE_ROUTE_STATE] ??= {
    routes: new Map(),
    unrouted: new Map(),
    warnedUnrouted: new Set(),
  });
}

/**
 * Bound on completions held for sessions no Location has registered, so a
 * session that never comes back cannot grow the table without limit. A
 * completion evicted here stays unacknowledged in the bridge.
 */
const MAX_UNROUTED_COMPLETIONS = 256;

/**
 * Delays before each automatic retry of a standalone wake (a pushed
 * completion or watch match delivered without a SubC pool) whose delivery
 * failed, most often because the host refused synthetic input for a while.
 * Without SubC nothing nudges a later drain, and a once-watch stops scanning
 * after its first match, so without these retries the result would wait for
 * the session's next registration. After the last delay the wake is still
 * kept for that registration.
 */
const STANDALONE_RETRY_DELAYS_MS: readonly number[] = [
  500, 1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000,
];

export interface V2RuntimeConsumerOptions {
  /** Delays before each automatic retry of a failed standalone wake; tests shorten them. */
  readonly standaloneRetryDelaysMs?: readonly number[];
}

function holdUnrouted(completion: BashCompletedPayload, bridge: AftProjectTransport): void {
  const { unrouted } = wakeRouteState();
  let held = 0;
  for (const tasks of unrouted.values()) held += tasks.size;
  const sessionTasks = unrouted.get(completion.session_id);
  if (!sessionTasks?.has(completion.task_id) && held >= MAX_UNROUTED_COMPLETIONS) {
    // Map iteration follows insertion order, so this drops every completion
    // of the session that was first held and has waited longest.
    const oldest = unrouted.keys().next();
    if (!oldest.done) unrouted.delete(oldest.value);
  }
  const tasks = unrouted.get(completion.session_id) ?? new Map<string, UnroutedCompletion>();
  tasks.set(completion.task_id, { completion, bridge });
  unrouted.set(completion.session_id, tasks);
}

function forgetUnroutedCompletion(sessionID: string, taskID: string): void {
  const state = wakeRouteState();
  const held = state.unrouted.get(sessionID);
  if (!held) return;
  held.delete(taskID);
  if (held.size === 0) {
    state.unrouted.delete(sessionID);
    state.warnedUnrouted.delete(sessionID);
  }
}

function warnUnroutedOnce(sessionID: string, message: string): void {
  const { warnedUnrouted } = wakeRouteState();
  if (warnedUnrouted.has(sessionID)) return;
  warnedUnrouted.add(sessionID);
  warn(message);
}

async function routeCompletion(
  completion: BashCompletedPayload,
  bridge: AftProjectTransport,
  acknowledge = true,
): Promise<boolean> {
  const route = wakeRouteState().routes.get(completion.session_id);
  if (route) {
    try {
      if (acknowledge && route.standaloneCompletion) {
        await route.standaloneCompletion(completion, bridge);
        forgetUnroutedCompletion(completion.session_id, completion.task_id);
        return true;
      }
      const admitted = (await route.completion(completion, bridge, acknowledge)) === true;
      // The held copy is an admission retry, not a second durable notification.
      // bg-notifications.ts tracks subc acknowledgement retries separately in
      // deliveredAwaitingAckTaskIds after the host accepts the completion.
      if (admitted) forgetUnroutedCompletion(completion.session_id, completion.task_id);
      return admitted;
    } catch (error) {
      // Subc drains retain failed admissions in their shared retry queue. An
      // unrouted copy could also wake the session after an async watch converts
      // the queued completion to a task-exit notice, duplicating its wake/ack.
      // Only pushed completions need a separate session-registration retry.
      if (acknowledge) holdUnrouted(completion, bridge);
      warn(
        `[bash_completion] delivery of task ${completion.task_id} to session ${completion.session_id} failed: ${error instanceof Error ? error.message : String(error)}`,
      );
      return false;
    }
  }

  // Drained completions stay in the shared retry queue until a Location
  // registers. Only pushed completions need a local copy for registration.
  if (acknowledge) holdUnrouted(completion, bridge);
  warnUnroutedOnce(
    completion.session_id,
    `[bash_completion] no OpenCode Location has session ${completion.session_id} registered; completion of task ${completion.task_id} is held unacknowledged until the session registers`,
  );
  return false;
}

async function routeLongRunning(
  reminder: BashLongRunningPayload,
  _bridge: AftProjectTransport,
): Promise<void> {
  const route = wakeRouteState().routes.get(reminder.session_id);
  if (route) return route.longRunning(reminder);

  // A long-running reminder only says a task is still running; holding it for
  // later delivery could report a task that has finished since, so it is not held.
  warnUnroutedOnce(
    reminder.session_id,
    `[bash_long_running] no OpenCode Location has session ${reminder.session_id} registered; long-running reminder for task ${reminder.task_id} was not delivered`,
  );
}

/**
 * Deliver the completions held for a session that just registered with
 * `route`. The bridge pushes each completion once, so without this a
 * completion that arrived while its Location was not registered (for example
 * during a Location reload) would stay pending and never wake the session.
 */
function deliverUnrouted(sessionID: string, replacementBridge?: AftProjectTransport): void {
  const state = wakeRouteState();
  state.warnedUnrouted.delete(sessionID);
  const held = state.unrouted.get(sessionID);
  if (!held) return;
  state.unrouted.delete(sessionID);
  for (const { completion, bridge } of held.values()) {
    if (replacementBridge && bridge.getCwd() !== replacementBridge.getCwd()) continue;
    void routeCompletion(completion, replacementBridge ?? bridge);
  }
}

/** Test-only: forget every route and held completion in the process-wide table. */
export function __resetV2WakeRoutesForTests(): void {
  const state = wakeRouteState();
  state.routes.clear();
  state.unrouted.clear();
  state.warnedUnrouted.clear();
}

async function routePatternMatch(
  match: PatternMatchEntry,
  bridge: AftProjectTransport,
): Promise<void> {
  // A bridge can push frames for sessions owned by another host. Never admit
  // or acknowledge those frames on behalf of a registered local session.
  await wakeRouteState().routes.get(match.session_id)?.standalonePatternMatch?.(match, bridge);
}

const sharedBridgeOptions: Pick<
  BridgeOptions,
  "onBashCompletion" | "onBashLongRunning" | "onBashPatternMatch"
> = {
  onBashCompletion: async (completion, bridge) => {
    await routeCompletion(completion, bridge);
  },
  onBashLongRunning: routeLongRunning,
  onBashPatternMatch: routePatternMatch,
};

const sharedTransportOptions: Pick<AftTransportFactoryOptions, "onBgEventsNudge"> = {
  onBgEventsNudge: (root, sessionID) => {
    void wakeRouteState().routes.get(sessionID)?.drainPatterns?.(sessionID, root);
  },
};

function isCompletionEntry(value: unknown): value is BashCompletedPayload {
  if (typeof value !== "object" || value === null) return false;
  const entry = value as Partial<BashCompletedPayload>;
  return (
    typeof entry.session_id === "string" &&
    typeof entry.task_id === "string" &&
    typeof entry.command === "string" &&
    typeof entry.status === "string" &&
    (entry.exit_code === null || typeof entry.exit_code === "number")
  );
}

/**
 * One entry of a `bash_drain_completions` reply, as a completion of
 * `sessionID`. The reply lists only the requested session's completions, and
 * the daemon serializes each entry without `session_id`: only push frames carry
 * it. A check that requires the field drops every drained completion, so a
 * pushed completion would fail its replay check and never wake the session. An
 * entry that names a different session is still refused.
 */
function drainedCompletion(value: unknown, sessionID: string): BashCompletedPayload | undefined {
  if (typeof value !== "object" || value === null) return undefined;
  const entry = value as Partial<BashCompletedPayload>;
  if (entry.session_id !== undefined && entry.session_id !== sessionID) return undefined;
  const completion = { ...entry, type: "bash_completed" as const, session_id: sessionID };
  return isCompletionEntry(completion) ? completion : undefined;
}

export interface V2RuntimeConsumer extends V2ToolConsumers {
  readonly bridgeOptions: typeof sharedBridgeOptions;
  readonly transportOptions: typeof sharedTransportOptions;
  dispose(): void;
  subcWakes(
    getPool: () => AftTransportPool,
    directory: string,
  ): {
    handlers: Pick<AftTransportFactoryOptions, "onBgEventsNudge" | "onBgEventsNudgeRef">;
    drainSession(sessionID: string): Promise<void>;
    restoreSession(sessionID: string): Promise<void>;
  };
}

/**
 * Detach the waits of the session an admitted completion wake belongs to.
 *
 * On OpenCode 1 a completion wake reaches the session as a synthetic message
 * part, and `chat.message` detaches a waiting `bash({wait:true})` and aborts a
 * sync `bash_watch` for it, so the agent can act on the result instead of
 * sitting out the wait. OpenCode 2 admits the wake through
 * `session.synthetic`, which does not run the session `prompt` hook where AFT
 * makes that decision for typed messages, so this code applies the shared
 * detachment decision to the wake as a synthetic part. Waits detach only when
 * `bash.detach_on_user_message` is on.
 *
 * The wait is asked to detach on the bridge that pushed the completion: a
 * session's background task and its waiting call run on the bridge of the
 * Location the session belongs to.
 */
function detachWaitsForCompletion(
  sessionID: string,
  text: string,
  bridge: AftProjectTransport,
  config: AftConfig,
): void {
  const onlyThisBridge = {
    getActiveBridgeForRoot: () => bridge,
    activeBridges: () => [bridge],
  } as unknown as Parameters<typeof interruptBashWaitsForChatMessage>[0];
  interruptBashWaitsForChatMessage(
    onlyThisBridge,
    config,
    "",
    sessionID,
    {
      parts: [{ type: "text", text, synthetic: true }],
    },
    false,
  );
}

/**
 * Creates the V2 execution consumers for one OpenCode Location. The bridge owns
 * one process-wide `onBashCompletion` callback, so it routes each notification
 * by session to the delivery object owned by that Location's Effect scope.
 *
 * `getConfig` returns the Location's current AFT config (a live reload
 * replaces it); it decides whether an admitted completion detaches the
 * session's waits.
 */
export function createV2RuntimeConsumer(
  context: V2WakeHostContext,
  getConfig: () => AftConfig = () => ({}),
  getBridge?: (root?: string) => AftProjectTransport,
  options: V2RuntimeConsumerOptions = {},
): V2RuntimeConsumer {
  const delivery = new V2SessionDelivery(context.session);
  // SubC owns acknowledgement retries outside admission. Keep its existing
  // completion and non-batched watch keys. Batched watch keys clear when an
  // acknowledgement is confirmed or the watch is absent from the pending list.
  const subcDelivery = new V2SessionDelivery(context.session, Number.POSITIVE_INFINITY);
  const sessions = new Set<string>();
  let subcPool: (() => AftTransportPool) | undefined;
  const pendingMatches = new Map<
    string,
    {
      match: PatternMatchEntry;
      bridge: AftProjectTransport;
      admitted: boolean;
    }
  >();
  const completing = new Map<string, Promise<void>>();
  const matching = new Map<string, Promise<void>>();
  const eachLineWork = new Map<
    string,
    {
      version: number;
      latest?: { match: PatternMatchEntry; bridge: AftProjectTransport };
      promise: Promise<void>;
    }
  >();
  const lineProgress = new Map<string, WatchBatchProgress>();
  const subcLineProgress = new Map<string, WatchBatchProgress>();
  const subcMatchID = (watch: string, version: number) => `match\0${watch}\0${version}`;
  const retireSubcBatchVersion = (
    sessionID: string,
    task: string,
    watch: string,
    version: number,
  ) =>
    subcDelivery.retireCompletion({
      sessionID,
      taskIDs: [task],
      notificationID: subcMatchID(watch, version),
    });
  const watchKey = (match: PatternMatchEntry) =>
    JSON.stringify([match.session_id, match.task_id, match.watch_id]);
  const matchKey = (match: PatternMatchEntry) =>
    JSON.stringify([
      match.session_id,
      match.task_id,
      match.watch_id,
      match.match_offset,
      match.reason,
    ]);
  const draining = new Map<string, Promise<void>>();
  let disposed = false;
  const retryDelays = options.standaloneRetryDelaysMs ?? STANDALONE_RETRY_DELAYS_MS;
  /**
   * Automatic retries of failed standalone wakes, keyed like `completing`
   * (completions) and `pendingMatches` (watch matches). `attempts` counts
   * the retries already scheduled; `timer` is set while one is waiting.
   */
  const retries = new Map<
    string,
    { attempts: number; timer?: ReturnType<typeof setTimeout> | undefined }
  >();
  const clearRetry = (key: string): void => {
    const retry = retries.get(key);
    if (retry?.timer !== undefined) clearTimeout(retry.timer);
    retries.delete(key);
  };
  /**
   * Retry a failed standalone wake after the next delay. A wake that already
   * has a retry waiting keeps it: a failed attempt from another trigger (a
   * session registration) neither adds a second timer nor uses up a delay.
   * Each retry goes through the same delivery path as the first attempt,
   * with its in-flight, admission and durable-pending checks, so it cannot
   * wake the session twice. After the last delay this logs and stops; the
   * wake stays held for the session's next registration.
   */
  const scheduleRetry = (key: string, sessionID: string, what: string, run: () => void): void => {
    if (disposed || v2SessionDeleted(context, sessionID)) {
      clearRetry(key);
      return;
    }
    const retry = retries.get(key) ?? { attempts: 0 };
    if (retry.timer !== undefined) return;
    const delay = retryDelays[retry.attempts];
    if (delay === undefined) {
      retries.delete(key);
      warn(
        `[bash_wake] gave up retrying ${what} for session ${sessionID} after ${retry.attempts} retries; it stays pending until the session registers again`,
      );
      return;
    }
    retry.attempts += 1;
    retry.timer = setTimeout(() => {
      retry.timer = undefined;
      if (disposed || retries.get(key) !== retry) return;
      // A session deleted meanwhile still goes through `run`: the delivery
      // path then acknowledges the wake without delivering it and ends the
      // retries.
      run();
    }, delay);
    // A waiting retry must not keep the host process alive on its own.
    (retry.timer as { unref?: () => void }).unref?.();
    retries.set(key, retry);
  };
  const retireOlderEachLine = (match: PatternMatchEntry): boolean => {
    const watch = watchKey(match);
    for (const [key, pending] of pendingMatches) {
      if (!pending.match.batch || watchKey(pending.match) !== watch) continue;
      if (pending.match.match_offset > match.match_offset) return false;
      if (pending.match.match_offset < match.match_offset) {
        pendingMatches.delete(key);
        clearRetry(key);
        delivery.retirePatternMatch({
          sessionID: pending.match.session_id,
          taskIDs: [pending.match.task_id],
          watchID: pending.match.watch_id,
          matchOffset: pending.match.match_offset,
          reason: pending.match.reason,
          text: "",
        });
      }
    }
    return true;
  };
  const deliverStandalonePatternMatch = async (
    match: PatternMatchEntry,
    bridge: AftProjectTransport,
  ): Promise<void> => {
    const key = matchKey(match);
    const watch = watchKey(match);
    // As in `patternMatch`: a deleted session has no one to tell. Acknowledge
    // the match without queueing it, so the bridge does not deliver it again.
    if (v2SessionDeleted(context, match.session_id)) {
      pendingMatches.delete(key);
      if (match.batch) lineProgress.delete(watch);
      clearRetry(key);
      await bridge
        .send("bash_ack_completions", {
          session_id: match.session_id,
          ...(match.reason === "task_exit"
            ? { task_ids: [match.task_id] }
            : {
                matches: [
                  {
                    task_id: match.task_id,
                    watch_id: match.watch_id,
                    match_offset: match.match_offset,
                  } satisfies BashPatternMatchAck,
                ],
              }),
        })
        .catch((error) => {
          warn(
            `[bash_pattern_match] acknowledging watch ${match.watch_id} of deleted session ${match.session_id} failed: ${error instanceof Error ? error.message : String(error)}`,
          );
        });
      return;
    }
    const active = matching.get(key);
    if (active) return active;
    const pending = pendingMatches.get(key) ?? { match, bridge, admitted: false };
    pendingMatches.set(key, pending);
    const deliver = async () => {
      if (disposed) return;
      const isSubagent = await resolveIsSubagent(context, match.session_id);
      if (disposed) return;
      if (isSubagent) {
        await awaitV2ExecutionSettlement(context, match.session_id);
        if (disposed) return;
        if (!v2WatchWakeAllowed(context, match.session_id, match.task_id)) return;
      }
      let admittedBatch: BashWatchLineBatch | undefined;
      let progress: WatchBatchProgress | undefined;
      const wake = {
        sessionID: match.session_id,
        taskIDs: [match.task_id],
        watchID: match.watch_id,
        matchOffset: match.match_offset,
        reason: match.reason,
        text: formatPatternMatchReminder([match]),
      };
      // Rust reports a deleted task by sending a terminal notification, not
      // by saving a pending match. There is no task row in which to persist it.
      const erased = match.reason === "task_exit" && match.match_text === "watch target erased";
      if (!erased) {
        // A late push can arrive after its ack, or after a newer sticky match
        // replaced it. Durable pending state is the replay fence, not an
        // ever-growing set of every offset this consumer has delivered.
        const current = await pending.bridge.send("bash_drain_completions", {
          session_id: match.session_id,
        });
        if (disposed) return;
        if (current.success === false || !Array.isArray(current.pending_matches)) {
          throw new Error("pattern replay validation failed");
        }
        const durable = current.pending_matches.filter(isPatternMatchEntry);
        const currentMatch = durable.find(
          (entry) =>
            entry.session_id === match.session_id &&
            entry.task_id === match.task_id &&
            entry.watch_id === match.watch_id &&
            entry.match_offset === match.match_offset &&
            entry.reason === match.reason,
        );
        if (!currentMatch) {
          if (match.batch && !durable.some((entry) => watchKey(entry) === watch))
            lineProgress.delete(watch);
          delivery.retirePatternMatch(wake);
          pendingMatches.delete(key);
          return;
        }
        if (match.batch) {
          // A superseded push may never have reached the host. The durable
          // snapshot retains its rows; watermarks remove only admitted data.
          admittedBatch = {
            ...(currentMatch.batch ?? match.batch),
            snapshot: true,
            version: match.match_offset,
          };
          progress = lineProgress.get(watch) ?? new WatchBatchProgress();
          const unseen = progress.unseen(admittedBatch);
          wake.text = formatPatternMatchReminder([{ ...currentMatch, batch: unseen }]);
          if (!watchBatchHasContent(unseen)) pending.admitted = true;
        }
      }
      if (!pending.admitted) {
        const text = wake.text;
        const admitted = await Effect.runPromise(
          delivery.patternMatch({
            ...wake,
            metadata: { source: "aft", kind: "bash_pattern_match", watch_id: match.watch_id },
          }),
        );
        if (disposed || admitted === "not-delivered") return;
        pending.admitted = true;
        if (admittedBatch && progress) {
          progress.admit(admittedBatch);
          lineProgress.set(watch, progress);
        }
        if (admitted === "delivered")
          detachWaitsForCompletion(match.session_id, text, bridge, getConfig());
      }
      // The daemon deletes an acknowledged once-watch, but only clears a
      // sticky watch's pending match so it can keep scanning later output.
      const ack = await pending.bridge.send("bash_ack_completions", {
        session_id: match.session_id,
        ...(match.reason === "task_exit"
          ? { task_ids: [match.task_id] }
          : {
              matches: [
                {
                  task_id: match.task_id,
                  watch_id: match.watch_id,
                  match_offset: match.match_offset,
                } satisfies BashPatternMatchAck,
              ],
            }),
      });
      if (disposed) return;
      if (ack.success === false)
        throw new Error(String(ack.message ?? "pattern acknowledgement failed"));
      if (
        match.reason !== "task_exit" &&
        (!Array.isArray(ack.acked_matches) ||
          !ack.acked_matches.some((entry) => {
            if (typeof entry !== "object" || entry === null) return false;
            const confirmed = entry as Partial<BashPatternMatchAck>;
            return (
              confirmed.task_id === match.task_id &&
              confirmed.watch_id === match.watch_id &&
              confirmed.match_offset === match.match_offset
            );
          }))
      ) {
        // Older engines may ignore the matches parameter. Keep the admission
        // reserved until a later drain proves this match is no longer pending.
        throw new Error("pattern acknowledgement was not confirmed");
      }
      pendingMatches.delete(key);
      if (match.batch) lineProgress.delete(watch);
      delivery.retirePatternMatch(wake);
    };
    const attempt = deliver()
      .then(
        () => clearRetry(key),
        (error) => {
          warn(
            `[bash_pattern_match] delivery of watch ${match.watch_id} to session ${match.session_id} failed: ${error instanceof Error ? error.message : String(error)}`,
          );
          // The match stays in `pendingMatches`. Retry it from there, unless
          // something else delivered or retired it first.
          scheduleRetry(key, match.session_id, `watch ${match.watch_id} match`, () => {
            const current = pendingMatches.get(key);
            if (current) void route.standalonePatternMatch?.(current.match, current.bridge);
            else clearRetry(key);
          });
        },
      )
      .finally(() => matching.delete(key));
    matching.set(key, attempt);
    return attempt;
  };
  const route: WakeRoute = {
    standalonePatternMatch: async (match, bridge) => {
      if (!match.batch) return deliverStandalonePatternMatch(match, bridge);
      const watch = watchKey(match);
      const active = eachLineWork.get(watch);
      if (active) {
        if (match.match_offset > (active.latest?.match.match_offset ?? active.version))
          active.latest = { match, bridge };
        return;
      }
      if (!retireOlderEachLine(match)) return;
      // One active admission and one latest version bound a noisy watch's
      // backlog. The durable snapshot accounts for coalesced versions' rows.
      const work: {
        version: number;
        latest?: { match: PatternMatchEntry; bridge: AftProjectTransport };
        promise: Promise<void>;
      } = {
        version: match.match_offset,
        latest: { match, bridge },
        promise: Promise.resolve(),
      };
      eachLineWork.set(watch, work);
      work.promise = Promise.resolve()
        .then(async () => {
          while (!disposed && work.latest) {
            const next = work.latest;
            work.latest = undefined;
            work.version = next.match.match_offset;
            if (retireOlderEachLine(next.match))
              await deliverStandalonePatternMatch(next.match, next.bridge);
          }
        })
        .finally(() => {
          if (eachLineWork.get(watch) === work) eachLineWork.delete(watch);
        });
      return work.promise;
    },
    drainPatterns: async (sessionID, root) => {
      // SubC owns session-event subscriptions and retries pending wakes after
      // host admission fails. A parallel standalone drain would open streams
      // outside SubC's recovery window and could deliver the same wake twice.
      if (!getBridge || disposed || subcPool) return;
      const drainKey = JSON.stringify([sessionID, root]);
      const active = draining.get(drainKey);
      if (active) return active;
      const drain = async () => {
        const bridge = getBridge(root);
        const response = await bridge.send("bash_drain_completions", { session_id: sessionID });
        if (response.success === false || disposed) return;
        for (const match of (Array.isArray(response.pending_matches)
          ? response.pending_matches
          : []
        ).filter(isPatternMatchEntry)) {
          // A drain is scoped to its requesting session, even if a malformed
          // response includes a frame for another registered session.
          if (match.session_id === sessionID) {
            await routePatternMatch(match, bridge);
            if (disposed) return;
          }
        }
        for (const entry of Array.isArray(response.bg_completions) ? response.bg_completions : []) {
          const completion = drainedCompletion(entry, sessionID);
          if (completion) {
            await routeCompletion(completion, bridge);
            if (disposed) return;
          }
        }
      };
      const attempt = drain()
        .catch((error) => {
          warn(
            `[bash_pattern_match] drain for session ${sessionID} failed: ${error instanceof Error ? error.message : String(error)}`,
          );
        })
        .finally(() => draining.delete(drainKey));
      draining.set(drainKey, attempt);
      return attempt;
    },
    completion: async (completion, bridge, acknowledge = true) => {
      if (
        !v2SessionDeleted(context, completion.session_id) &&
        (await resolveIsSubagent(context, completion.session_id))
      ) {
        await awaitV2ExecutionSettlement(context, completion.session_id);
      }
      // Settlement may be released by Location disposal or session deletion,
      // not just execution end. Do not admit a wake into a released Location.
      if (disposed) return false;
      // The host deleted this session. Holding or delivering the completion
      // would recreate notification state nothing releases again, so drop it
      // here. It is still acknowledged so the bridge does not deliver it again;
      // a drain (acknowledge false) acknowledges what this returns true for.
      if (v2SessionDeleted(context, completion.session_id)) {
        if (acknowledge) {
          await bridge.send("bash_ack_completions", {
            session_id: completion.session_id,
            task_ids: [completion.task_id],
          });
        }
        return true;
      }
      if (
        (await resolveIsSubagent(context, completion.session_id)) &&
        !v2WatchWakeAllowed(context, completion.session_id, completion.task_id)
      ) {
        holdBgCompletion(
          completion.session_id,
          completion,
          v2SubagentStopped(context, completion.session_id),
        );
        forgetV2Watch(context, completion.session_id, completion.task_id);
        return false;
      }
      const text = formatSystemReminder([completion]);
      const admitted = await Effect.runPromise(
        subcDelivery.completion({
          sessionID: completion.session_id,
          taskIDs: [completion.task_id],
          text,
          metadata: {
            source: "aft",
            kind: "bash_completion",
            task_ids: [completion.task_id],
            status: completion.status,
          },
        }),
      );
      // A wake that was not admitted (a staged revert defers it) reaches the
      // agent later, so it must not end a wait now.
      if (disposed || admitted === "not-delivered") return false;
      if (admitted === "delivered")
        detachWaitsForCompletion(completion.session_id, text, bridge, getConfig());

      if (acknowledge) {
        if (subcPool) {
          consumeBgCompletion(completion.session_id, completion.task_id);
          await markBgCompletionDelivered(
            {
              ctx: { pool: subcPool() },
              directory: bridge.getCwd(),
              sessionID: completion.session_id,
              resolvedBridge: bridge,
            },
            completion.task_id,
          );
        } else {
          await bridge.send("bash_ack_completions", {
            session_id: completion.session_id,
            task_ids: [completion.task_id],
          });
        }
      }
      for (const [key, pending] of pendingMatches) {
        const match = pending.match;
        if (match.session_id !== completion.session_id || match.task_id !== completion.task_id)
          continue;
        delivery.retirePatternMatch({
          sessionID: match.session_id,
          taskIDs: [match.task_id],
          watchID: match.watch_id,
          matchOffset: match.match_offset,
          reason: match.reason,
          text: "",
        });
        pendingMatches.delete(key);
        clearRetry(key);
      }
      forgetV2Watch(context, completion.session_id, completion.task_id);
      return true;
    },
    patternMatch: async (match, bridge) => {
      // A deleted session has no one to tell. Report the match handled so the
      // drain acknowledges it instead of queueing it for the session again.
      if (v2SessionDeleted(context, match.session_id)) return true;
      if (await resolveIsSubagent(context, match.session_id)) {
        await awaitV2ExecutionSettlement(context, match.session_id);
        if (!v2WatchWakeAllowed(context, match.session_id, match.task_id)) return false;
      }
      const watch = watchKey(match);
      const batch = match.batch
        ? { ...match.batch, snapshot: true, version: match.match_offset }
        : undefined;
      const progress = batch
        ? (subcLineProgress.get(watch) ?? new WatchBatchProgress())
        : undefined;
      const unseen = batch && progress ? progress.unseen(batch) : undefined;
      if (batch && progress && unseen && !watchBatchHasContent(unseen)) {
        progress.admit(batch);
        subcLineProgress.set(watch, progress);
        return true;
      }
      const text = formatPatternMatchReminder([unseen ? { ...match, batch: unseen } : match]);
      const admitted = await Effect.runPromise(
        subcDelivery.completion({
          sessionID: match.session_id,
          taskIDs: [match.task_id],
          notificationID: subcMatchID(match.watch_id, match.match_offset),
          text,
          metadata: {
            source: "aft",
            kind: "bash_pattern_match",
            task_ids: [match.task_id],
            watch_id: match.watch_id,
          },
        }),
      );
      if (disposed || admitted === "not-delivered") return false;
      if (batch && progress) {
        progress.admit(batch);
        subcLineProgress.set(watch, progress);
      }
      if (admitted === "delivered") {
        detachWaitsForCompletion(match.session_id, text, bridge, getConfig());
        if (match.reason === "task_exit") forgetUnroutedCompletion(match.session_id, match.task_id);
      }
      return true;
    },
    standaloneCompletion: async (completion, bridge) => {
      if (subcPool) {
        await route.completion(completion, bridge);
        return;
      }
      const key = JSON.stringify([completion.session_id, completion.task_id]);
      // As in `completion`: a deleted session gets no wake and no per-session
      // state, but the completion is still acknowledged so the bridge does not
      // deliver it again.
      if (v2SessionDeleted(context, completion.session_id)) {
        clearRetry(key);
        await bridge.send("bash_ack_completions", {
          session_id: completion.session_id,
          task_ids: [completion.task_id],
        });
        return;
      }
      const active = completing.get(key);
      if (active) return active;
      const deliver = async () => {
        if (disposed) return;
        const isSubagent = await resolveIsSubagent(context, completion.session_id);
        if (disposed) return;
        if (isSubagent) {
          await awaitV2ExecutionSettlement(context, completion.session_id);
          if (disposed) return;
        }
        if (isSubagent && !v2WatchWakeAllowed(context, completion.session_id, completion.task_id)) {
          holdBgCompletion(
            completion.session_id,
            completion,
            v2SubagentStopped(context, completion.session_id),
          );
          forgetV2Watch(context, completion.session_id, completion.task_id);
          return;
        }
        // Check the daemon's pending completions before admitting a wake: a
        // queued duplicate may arrive after its ACK removed the in-memory key.
        const current = await bridge.send("bash_drain_completions", {
          session_id: completion.session_id,
        });
        if (disposed) return;
        if (current.success === false || !Array.isArray(current.bg_completions))
          throw new Error("completion replay validation failed");
        const wake = { sessionID: completion.session_id, taskIDs: [completion.task_id] };
        if (
          !current.bg_completions.some(
            (entry) =>
              drainedCompletion(entry, completion.session_id)?.task_id === completion.task_id,
          )
        ) {
          delivery.retireCompletion(wake);
          return;
        }
        const text = formatSystemReminder([completion]);
        const admitted = await Effect.runPromise(
          delivery.completion({
            sessionID: completion.session_id,
            taskIDs: [completion.task_id],
            text,
            metadata: {
              source: "aft",
              kind: "bash_completion",
              task_ids: [completion.task_id],
              status: completion.status,
            },
          }),
        );
        if (disposed || admitted === "not-delivered") return;
        // An already admitted wake still needs another ACK attempt if its earlier ACK failed.
        if (admitted === "delivered")
          detachWaitsForCompletion(completion.session_id, text, bridge, getConfig());

        const ack = await bridge.send("bash_ack_completions", {
          session_id: completion.session_id,
          task_ids: [completion.task_id],
        });
        if (disposed) return;
        if (
          ack.success === false ||
          !Array.isArray(ack.acked_task_ids) ||
          !ack.acked_task_ids.includes(completion.task_id)
        )
          throw new Error("completion acknowledgement was not confirmed");
        delivery.retireCompletion(wake);
        for (const [key, pending] of pendingMatches) {
          const match = pending.match;
          if (match.session_id !== completion.session_id || match.task_id !== completion.task_id)
            continue;
          delivery.retirePatternMatch({
            sessionID: match.session_id,
            taskIDs: [match.task_id],
            watchID: match.watch_id,
            matchOffset: match.match_offset,
            reason: match.reason,
            text: "",
          });
          pendingMatches.delete(key);
          clearRetry(key);
        }
        forgetV2Watch(context, completion.session_id, completion.task_id);
      };
      const attempt = deliver()
        .then(
          () => clearRetry(key),
          (error) => {
            // `routeCompletion` holds the failed completion for the session's
            // next registration and logs the failure; retry it sooner here.
            scheduleRetry(
              key,
              completion.session_id,
              `completion of task ${completion.task_id}`,
              () => {
                void routeCompletion(completion, bridge);
              },
            );
            throw error;
          },
        )
        .finally(() => completing.delete(key));
      completing.set(key, attempt);
      return attempt;
    },
    longRunning: (reminder) =>
      Effect.runPromise(
        delivery.status({
          sessionID: reminder.session_id,
          text: formatLongRunningReminder([reminder]),
          description: "Background bash status",
          metadata: {
            source: "aft",
            kind: "bash_long_running",
            task_id: reminder.task_id,
          },
        }),
      ),
  };

  const registerSession = (sessionID: string | undefined, bridge?: AftProjectTransport): void => {
    if (!sessionID || disposed) return;
    sessions.add(sessionID);
    const { routes } = wakeRouteState();
    routes.set(sessionID, route);
    deliverUnrouted(sessionID, bridge);
    for (const { match, bridge } of pendingMatches.values()) {
      if (match.session_id === sessionID) void route.standalonePatternMatch?.(match, bridge);
    }
    void route.drainPatterns?.(sessionID);
  };

  return {
    bridgeOptions: sharedBridgeOptions,
    transportOptions: sharedTransportOptions,
    registerSession,
    subcWakes: (getPool, directory) => {
      subcPool = getPool;
      const root = canonicalizeProjectRoot(directory);
      const drain = (
        sessionID: string,
        nudgeRef?: BgNudgeRef,
        resolvedBridge?: AftProjectTransport,
      ) => {
        if (disposed) return Promise.resolve();
        const drained = handleSubcBgEventsNudge({
          ctx: { pool: getPool() },
          directory: root,
          sessionID,
          client: context,
          nudgeRef,
          resolvedBridge,
          delivery: {
            completion: (completion, bridge) =>
              routeCompletion(
                { ...completion, type: "bash_completed", session_id: sessionID },
                bridge,
                false,
              ),
            patternMatch: async (match, bridge) => {
              if (match.session_id !== sessionID) return false;
              return (
                (await wakeRouteState().routes.get(sessionID)?.patternMatch?.(match, bridge)) ??
                false
              );
            },
            patternMatchAcked: (sessionID, match) => {
              if (!sessionID) return;
              const key = JSON.stringify([sessionID, match.task_id, match.watch_id]);
              const progress = subcLineProgress.get(key);
              if (!progress) return;
              retireSubcBatchVersion(sessionID, match.task_id, match.watch_id, match.match_offset);
              if (progress.admittedVersion === match.match_offset) subcLineProgress.delete(key);
            },
            patternMatchesPending: (sessionID, matches) => {
              const pending = new Map(
                matches.filter((match) => match.batch).map((match) => [watchKey(match), match]),
              );
              for (const [key, progress] of subcLineProgress) {
                const [session, task, watch] = JSON.parse(key) as [string, string, string];
                if (session !== sessionID) continue;
                const current = pending.get(key);
                if (
                  progress.admittedVersion !== undefined &&
                  current?.match_offset !== progress.admittedVersion
                ) {
                  retireSubcBatchVersion(session, task, watch, progress.admittedVersion);
                }
                if (!current) subcLineProgress.delete(key);
              }
            },
          },
        });
        // A drain keeps its own per-session bookkeeping while it delivers and
        // acknowledges. For a deleted session, release it once the drain ends.
        return drained.finally(() => {
          if (v2SessionDeleted(context, sessionID)) releaseSessionState(sessionID);
        });
      };
      return {
        handlers: {
          onBgEventsNudge: (projectRoot, sessionID) => {
            if (canonicalizeProjectRoot(projectRoot) === root) return drain(sessionID);
          },
          onBgEventsNudgeRef: (ref) => {
            if (ref.canonicalRoot === root) return drain(ref.session, ref);
          },
        },
        drainSession: (sessionID) => {
          if (disposed) return Promise.resolve();
          const bridge = getPool().getBridge(root);
          registerSession(sessionID, bridge);
          return drain(sessionID, undefined, bridge);
        },
        restoreSession: async (sessionID) => {
          if (disposed) return;
          const bridge = getPool().getBridge(root);
          registerSession(sessionID, bridge);
          await drain(sessionID, undefined, bridge);
        },
      };
    },
    onToolAbort: (sessionID) => noteV2ToolAbort(context, sessionID),
    executeBash: (execution) => {
      registerSession(execution.context.sessionID);
      return executeV2Bash(execution);
    },
    dispose: () => {
      disposed = true;
      for (const key of [...retries.keys()]) clearRetry(key);
      pendingMatches.clear();
      eachLineWork.clear();
      lineProgress.clear();
      subcLineProgress.clear();
      delivery.dispose();
      subcDelivery.dispose();
      const { routes } = wakeRouteState();
      // Another Location may have registered the session since; leave its route.
      for (const sessionID of sessions) {
        if (routes.get(sessionID) === route) routes.delete(sessionID);
      }
      sessions.clear();
    },
  };
}
