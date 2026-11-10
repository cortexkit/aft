import { Cause, Effect, Stream } from "effect";
import { markHeldBgCompletions } from "./bg-notifications.js";
import { error as logError, warn } from "./logger.js";
import { releaseSessionState } from "./shared/session-cleanup.js";
import { clearSessionDirectory } from "./shared/session-directory.js";
import { resolveSubagentStatus } from "./shared/subagent-detect.js";
import { callBashBridge } from "./tools/_shared.js";
import type { PluginContext } from "./types.js";

interface SessionWatches {
  stopped: boolean;
  stopEpoch: number;
  tasks: Map<string, () => Promise<void>>;
  settling?: { promise: Promise<void>; resolve: () => void };
  wakes: Set<string>;
  watches: Map<string, { taskId: string; cancel: () => Promise<unknown> }>;
}

/** Hold a racing wake until the host classifies an interrupted tool's execution. */
export function noteV2ToolAbort(client: unknown, sessionID: string): void {
  const state = stateFor(client, sessionID);
  if (!state || state.settling) return;
  let resolve = () => {};
  const promise = new Promise<void>((done) => {
    resolve = done;
  });
  state.settling = { promise, resolve };
}

export async function awaitV2ExecutionSettlement(
  client: unknown,
  sessionID: string,
): Promise<void> {
  await stateFor(client, sessionID, false)?.settling?.promise;
}
const locations = new WeakMap<object, Map<string, SessionWatches>>();
/**
 * Sessions the host has deleted, per Location. A background spawn or watch
 * registration can answer after the delete event; without this record that
 * late reply would recreate the session's entry and nothing would remove it
 * again. Capped so the record itself cannot grow without bound.
 */
const deletedSessions = new WeakMap<object, Set<string>>();
const DELETED_SESSIONS_MAX = 1_000;

function sessionDeleted(client: unknown, sessionID: string): boolean {
  return (
    !!client && typeof client === "object" && deletedSessions.get(client)?.has(sessionID) === true
  );
}

/**
 * True once the host has deleted this session in this Location. Wake routes
 * use it to drop a late completion without recreating per-session state.
 */
export function v2SessionDeleted(client: unknown, sessionID: string): boolean {
  return sessionDeleted(client, sessionID);
}

function rememberDeleted(client: object, sessionID: string): void {
  let deleted = deletedSessions.get(client);
  if (!deleted) {
    deleted = new Set();
    deletedSessions.set(client, deleted);
  }
  deleted.add(sessionID);
  if (deleted.size > DELETED_SESSIONS_MAX) {
    const oldest = deleted.values().next().value;
    if (oldest !== undefined) deleted.delete(oldest);
  }
}

/** Test-only visibility into retained session entries, including task-kill closures. */
export function _v2SessionCountForTest(client: object): number {
  return locations.get(client)?.size ?? 0;
}

function stateFor(client: unknown, sessionID: string, create = true): SessionWatches | undefined {
  if (!client || typeof client !== "object") return;
  const sessions = locations.get(client);
  if (!sessions) return;
  let state = sessions.get(sessionID);
  if (!state && sessionDeleted(client, sessionID)) {
    // A deleted session gets a detached, already-stopped state: late task and
    // watch registrations see `stopped` and kill or cancel what they started,
    // and the state is never stored, so it cannot leak.
    return create
      ? { stopped: true, stopEpoch: 0, tasks: new Map(), wakes: new Set(), watches: new Map() }
      : undefined;
  }
  if (!state && create) {
    state = {
      stopped: false,
      stopEpoch: 0,
      tasks: new Map(),
      wakes: new Set(),
      watches: new Map(),
    };
    sessions.set(sessionID, state);
  }
  return state;
}

export function v2SubagentStopped(client: unknown, sessionID: string): boolean {
  return stateFor(client, sessionID, false)?.stopped === true;
}

export function v2StopEpoch(client: unknown, sessionID: string): number {
  return stateFor(client, sessionID)?.stopEpoch ?? 0;
}

async function killBackgroundTask(
  ctx: PluginContext,
  runtime: { sessionID: string; directory: string },
  taskId: string,
): Promise<void> {
  const result = await callBashBridge(
    ctx,
    runtime,
    "bash_kill",
    { task_id: taskId },
    { abortSignal: undefined },
  );
  if (result.success === false) throw new Error(String(result.message ?? "bash_kill failed"));
  // The reply may carry completions that recreate per-session notification
  // state; a deleted session must not keep any.
  if (sessionDeleted(ctx.client, runtime.sessionID)) {
    releaseSessionState(runtime.sessionID);
    return;
  }
  // A kill reply can drain the completion before its push callback runs.
  // Keep that existing notice queued for a resumed tool result too.
  markHeldBgCompletions(runtime.sessionID);
}

/** Only a command's spawning call records ownership; watching another session's task does not. */
export async function trackV2BackgroundTask(
  ctx: PluginContext,
  runtime: { sessionID: string; directory: string },
  taskId: string,
  startedEpoch: number,
): Promise<void> {
  const state = stateFor(ctx.client, runtime.sessionID);
  if (!state) return;
  const kill = () => killBackgroundTask(ctx, runtime, taskId);
  // A background spawn can answer after its tool was interrupted, even after
  // a new execution started. A deliberate stop still owns that late task.
  if (state.stopped || state.stopEpoch !== startedEpoch) {
    await kill();
    return;
  }
  state.tasks.set(taskId, kill);
}

async function killStoppedBackgroundTasks(
  client: object,
  sessionID: string,
  ctx?: PluginContext,
  startedBeforeMs?: number,
): Promise<void> {
  const kills: Array<Promise<void>> = [];
  const visited = new Set<string>();
  const dispatch = (taskId: string, kill: () => Promise<void>) => {
    if (visited.has(taskId)) return;
    visited.add(taskId);
    kills.push(
      kill().catch((error) => {
        warn(
          `[bash] failed to kill stopped session ${sessionID}'s task ${taskId}: ${String(error)}`,
        );
      }),
    );
  };
  for (const [owner, state] of locations.get(client) ?? []) {
    if (owner !== sessionID) continue;
    const tasks = [...state.tasks];
    state.tasks.clear();
    for (const [taskId, kill] of tasks) {
      dispatch(taskId, kill);
    }
  }
  if (ctx) {
    // Persisted tasks survive a plugin reload. Bind the query and each kill
    // as the stopped owner; task records do not prove parent-child links.
    const runtime = {
      sessionID,
      directory: (client as { location: { directory: string } }).location.directory,
      directoryIsSessionRoot: true,
    };
    try {
      const result = await callBashBridge(
        ctx,
        runtime,
        "bash_owned_tasks",
        { started_before_ms: startedBeforeMs },
        { abortSignal: undefined },
      );
      if (result.success === false)
        throw new Error(String(result.message ?? "bash_owned_tasks failed"));
      for (const taskId of Array.isArray(result.task_ids) ? result.task_ids : []) {
        if (typeof taskId === "string")
          dispatch(taskId, () => killBackgroundTask(ctx, runtime, taskId));
      }
    } catch (error) {
      warn(`[bash] failed to enumerate stopped session ${sessionID}'s tasks: ${String(error)}`);
    }
  }
  await Promise.all(kills);
}

export function allowV2WatchWake(client: unknown, sessionID: string, taskId: string): void {
  const state = stateFor(client, sessionID);
  if (state && !state.stopped) state.wakes.add(taskId);
}

export function v2WatchWakeAllowed(client: unknown, sessionID: string, taskId: string): boolean {
  const state = stateFor(client, sessionID, false);
  return state !== undefined && !state.stopped && state.wakes.has(taskId);
}

export function forgetV2Watch(client: unknown, sessionID: string, taskId: string): void {
  const state = stateFor(client, sessionID, false);
  if (!state) return;
  state.wakes.delete(taskId);
  state.tasks.delete(taskId);
  for (const [watchId, watch] of state.watches) {
    if (watch.taskId === taskId) state.watches.delete(watchId);
  }
}

export async function trackV2Watch(
  ctx: PluginContext,
  runtime: { sessionID: string; directory: string },
  taskId: string,
  watchId: unknown,
): Promise<void> {
  const state = stateFor(ctx.client, runtime.sessionID);
  if (!state || typeof watchId !== "string") return;
  const cancel = () =>
    callBashBridge(
      ctx,
      runtime,
      "bash_unnotify",
      { task_id: taskId, watch_id: watchId },
      { abortSignal: undefined },
    );
  // Registration may finish after the execution's stop event was pulled.
  if (state.stopped) {
    await cancel();
    if (sessionDeleted(ctx.client, runtime.sessionID)) releaseSessionState(runtime.sessionID);
    return;
  }
  state.wakes.add(taskId);
  state.watches.set(watchId, { taskId, cancel });
}

/** Own the lazy host subscription before any tool can register a watch. */
export function registerV2SessionLifecycle(context: unknown, ctx?: PluginContext) {
  const host = context as {
    event?: {
      subscribe?: () => Stream.Stream<
        { type: string; created?: number; data: { sessionID?: string; reason?: string } },
        unknown
      >;
    };
  };
  return Effect.gen(function* () {
    if (!host.event?.subscribe || !context || typeof context !== "object") return;
    const sessions = new Map<string, SessionWatches>();
    locations.set(context, sessions);
    yield* Effect.addFinalizer(() =>
      Effect.sync(() => {
        for (const state of sessions.values()) state.settling?.resolve();
        locations.delete(context);
      }),
    );
    // toPull acquires the subscription; the scoped consumer starts pulling it
    // before registration returns, rather than merely constructing a Stream.
    const pull = yield* Stream.toPull(host.event.subscribe());
    yield* Effect.forkScoped(
      Effect.forever(
        pull.pipe(
          Effect.flatMap((events) =>
            Effect.forEach(
              events,
              (event) =>
                Effect.promise(async () => {
                  const sessionID = event.data.sessionID;
                  if (!sessionID) return;
                  if (event.type === "session.moved") {
                    clearSessionDirectory(sessionID);
                    return;
                  }
                  if (event.type === "session.deleted") {
                    const state = sessions.get(sessionID);
                    // Collect the in-memory kill targets before the entry is
                    // dropped; killStoppedBackgroundTasks reads them from it.
                    // It also asks the bridge for the tasks this session owns:
                    // after a plugin reload a running command may be known
                    // only there, with no in-memory entry for the session.
                    const kills = killStoppedBackgroundTasks(context, sessionID, ctx);
                    rememberDeleted(context, sessionID);
                    sessions.delete(sessionID);
                    releaseSessionState(sessionID);
                    if (!state) {
                      void kills.finally(() => releaseSessionState(sessionID));
                      return;
                    }
                    state.settling?.resolve();
                    state.settling = undefined;
                    state.stopped = true;
                    state.stopEpoch += 1;
                    state.wakes.clear();
                    const watches = [...state.watches.values()];
                    state.watches.clear();
                    // Like a deliberate stop, cleanup must not hold up the
                    // events that follow this one. Kill and cancel replies can
                    // carry completions that recreate notification state, so
                    // release it again once they have settled.
                    void Promise.all([
                      kills,
                      ...watches.map((watch) =>
                        watch
                          .cancel()
                          .catch((error) =>
                            warn(
                              `[bash_watch] failed to cancel deleted session ${sessionID}'s watch on ${watch.taskId}: ${String(error)}`,
                            ),
                          ),
                      ),
                    ]).finally(() => releaseSessionState(sessionID));
                    return;
                  }
                  if (!event.type.startsWith("session.execution.")) return;
                  const state = stateFor(context, sessionID);
                  if (!state) return;
                  if (event.type === "session.execution.started") {
                    state.stopped = false;
                    return;
                  }
                  const deliberateStop =
                    event.type === "session.execution.interrupted" &&
                    (event.data.reason === "user" || event.data.reason === "inactivity");
                  if (!deliberateStop) {
                    if (
                      event.type === "session.execution.failed" ||
                      event.type === "session.execution.succeeded" ||
                      event.type === "session.execution.interrupted"
                    ) {
                      state.settling?.resolve();
                      state.settling = undefined;
                    }
                    return;
                  }
                  // Only a proven primary is exempt: a failed lookup must not
                  // leave a stopped worker's tasks and watches alive.
                  if ((await resolveSubagentStatus(context, sessionID)) === false) {
                    state.settling?.resolve();
                    state.settling = undefined;
                    return;
                  }
                  state.stopped = true;
                  state.stopEpoch += 1;
                  state.wakes.clear();
                  state.settling?.resolve();
                  state.settling = undefined;
                  const watches = [...state.watches.values()];
                  state.watches.clear();
                  // Cleanup targets are captured now, but do not block later
                  // lifecycle events for a resumed execution while kills settle.
                  void Promise.all([
                    killStoppedBackgroundTasks(context, sessionID, ctx, event.created),
                    ...watches.map((watch) =>
                      watch.cancel().catch((error) => {
                        warn(
                          `[bash_watch] failed to cancel stopped session ${sessionID}'s watch on ${watch.taskId}: ${String(error)}`,
                        );
                      }),
                    ),
                  ]);
                }),
              { discard: true },
            ),
          ),
        ),
      ).pipe(
        Effect.catchCause((cause) =>
          Effect.sync(() => {
            if (!Cause.hasInterruptsOnly(cause)) {
              const directory = (context as { location?: { directory?: string } }).location
                ?.directory;
              // Without this subscription a stopped subagent's background
              // tasks and watches are never cleaned up, so say it stopped.
              logError(
                `AFT V2 session event consumer ended for Location ${directory ?? "(unknown)"}: ${Cause.pretty(cause)}`,
              );
            }
          }),
        ),
      ),
      { startImmediately: true },
    );
  });
}
