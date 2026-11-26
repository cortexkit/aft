import {
  type AftProjectTransport,
  type AftTransportPool,
  configErrorStatusSnapshot,
} from "@cortexkit/aft-bridge";
import { Effect, type Scope } from "effect";

import {
  type RpcNotification,
  registerNotificationSink,
  registerStatusChangeSink,
} from "../shared/rpc-notifications";
import { getSessionDirectoryForEvent } from "../shared/session-directory";
import { NOT_STARTED_STATUS_TEXT } from "../shared/status";
import { callBashBridge } from "../tools/_shared";
import type { PluginContext } from "../types";
import {
  type AftBackgroundTask,
  type AftBackgroundTaskSession,
  type AftHostToolOverlapNotice,
  type AftIndexProgress,
  AftRpc,
  type AftRpcSession,
  type AftStopBackgroundTaskInput,
  type AftStopBackgroundTaskResult,
} from "./contract";

type RpcRegistration = {
  dispose: Effect.Effect<void>;
  events: {
    emit(
      name: keyof typeof AftRpc.events,
      payload: Record<string, unknown>,
    ): Effect.Effect<void, unknown>;
  };
};

export type AftRpcContext = {
  location?: AftRpcLocation;
  session?: { get?: unknown };
  rpc: {
    register(
      definition: typeof AftRpc,
      handlers: {
        getStatus(input: AftRpcSession): Effect.Effect<Record<string, unknown>, unknown>;
        getBackgroundTasks(
          input: AftBackgroundTaskSession,
        ): Effect.Effect<AftBackgroundTask[], unknown>;
        stopBackgroundTask(
          input: AftStopBackgroundTaskInput,
        ): Effect.Effect<AftStopBackgroundTaskResult, unknown>;
      },
    ): Effect.Effect<RpcRegistration, unknown, Scope.Scope>;
  };
};

export type AftRpcLocation = {
  directory: string;
  workspaceID?: string;
};

export type RegisteredAftRpc = {
  emitStatusInvalidated(payload?: AftRpcSession): Promise<void>;
  emitShowStatusDialog(payload?: AftRpcSession): Promise<void>;
  emitIndexProgress(payload: AftIndexProgress): Promise<void>;
  /** A one-off user notice the OpenCode 2 TUI shows as a toast. */
  emitHostToolOverlap(payload: AftHostToolOverlapNotice): Promise<void>;
  dispose(): Promise<void>;
};

type StatusSubscribableTransport = AftProjectTransport & {
  subscribeStatus(listener: (snapshot: Record<string, unknown>) => void): () => void;
};

function asRecord(value: unknown): Record<string, unknown> | undefined {
  return typeof value === "object" && value !== null
    ? (value as Record<string, unknown>)
    : undefined;
}

function finiteNumber(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

function nonEmptyString(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

function sessionFromSnapshot(snapshot: Record<string, unknown>): string | undefined {
  return nonEmptyString(asRecord(snapshot.session)?.id);
}

export function indexProgressFromStatus(snapshot: Record<string, unknown>): AftIndexProgress[] {
  const sessionID = sessionFromSnapshot(snapshot);
  const progress: AftIndexProgress[] = [];
  const search = asRecord(snapshot.search_index);
  if (search && typeof search.status === "string") {
    progress.push({
      index: "search",
      status: search.status,
      ...(sessionID ? { sessionID } : {}),
      ...(finiteNumber(search.files) === undefined
        ? {}
        : { completed: finiteNumber(search.files) }),
    });
  }

  const semantic = asRecord(snapshot.semantic_index);
  if (semantic && typeof semantic.status === "string") {
    const stage = semantic.stage === null ? null : nonEmptyString(semantic.stage);
    const completed = finiteNumber(semantic.entries_done);
    const total = finiteNumber(semantic.entries_total);
    progress.push({
      index: "semantic",
      status: semantic.status,
      ...(sessionID ? { sessionID } : {}),
      ...(stage === undefined ? {} : { stage }),
      ...(completed === undefined ? {} : { completed }),
      ...(total === undefined ? {} : { total }),
    });
  }
  return progress;
}

function isStatusSubscribable(bridge: AftProjectTransport): bridge is StatusSubscribableTransport {
  return typeof (bridge as Partial<StatusSubscribableTransport>).subscribeStatus === "function";
}

/**
 * The root whose bridge answers status for this Location: the Location's own
 * directory, which is the root its tools resolve to (see `projectRootFor`,
 * which uses an OpenCode 2 runtime's `directory`). For a Location in a linked
 * git worktree that is the worktree. The project's canonical directory is the
 * main checkout, whose bridge (if any) holds another checkout's state, so
 * reading it would show the sidebar and footer "not initialized" or the wrong
 * project's counts (issue #387).
 */
function locationRoot(location: AftRpcLocation): string {
  return location.directory;
}

function placeholderStatus(): Record<string, unknown> {
  return {
    success: true,
    status: "not_initialized",
    cache_role: "not_initialized",
    message: NOT_STARTED_STATUS_TEXT,
  };
}

function restoreOwnProperty(
  target: object,
  key: PropertyKey,
  descriptor: PropertyDescriptor | undefined,
): void {
  if (descriptor) Object.defineProperty(target, key, descriptor);
  else Reflect.deleteProperty(target, key);
}

function taskSnapshot(reply: Record<string, unknown>, taskID: string, command: string) {
  if (
    reply.success === false &&
    (reply.code === "task_not_found" || reply.code === "task_erased")
  ) {
    return;
  }
  const { status } = reply;
  if (reply.success === true && reply.task_id === taskID) {
    switch (status) {
      case "starting":
      case "running":
      case "killing":
        return { active: true, status } as const;
      case "completed":
      case "failed":
      case "killed":
      case "timed_out":
      case "fate_unknown":
        return { active: false, status } as const;
    }
  }
  throw new Error(`${command} did not return a task snapshot`);
}

export function registerAftRpc(
  context: AftRpcContext,
  location: AftRpcLocation,
  pool: AftTransportPool,
  toolContext?: PluginContext,
): Effect.Effect<RegisteredAftRpc, unknown, Scope.Scope> {
  return Effect.gen(function* () {
    const root = locationRoot(location);
    const bridgeUnsubscribes = new Map<object, () => void>();
    let disposed = false;
    let registration!: RpcRegistration;

    const emit = async (
      name: keyof typeof AftRpc.events,
      payload: Record<string, unknown>,
    ): Promise<void> => {
      if (disposed) return;
      await Effect.runPromise(registration.events.emit(name, payload));
    };

    const publishStatus = (snapshot: Record<string, unknown>): void => {
      const sessionID = sessionFromSnapshot(snapshot);
      void emit("statusInvalidated", sessionID ? { sessionID } : {}).catch(() => {});
      for (const event of indexProgressFromStatus(snapshot)) {
        void emit("indexProgress", event).catch(() => {});
      }
    };

    const observeBridge = (bridge: AftProjectTransport | null): void => {
      if (!bridge || !isStatusSubscribable(bridge) || bridgeUnsubscribes.has(bridge)) return;
      bridgeUnsubscribes.set(bridge, bridge.subscribeStatus(publishStatus));
    };

    const getStatus = (input: AftRpcSession): Effect.Effect<Record<string, unknown>, unknown> =>
      Effect.suspend(() => {
        const sessionID = input.sessionID || "rpc";
        const bridge = pool.getActiveBridgeForRoot(root);
        if (!bridge) return Effect.succeed(placeholderStatus());
        observeBridge(bridge);

        const cached = bridge.getCachedStatus();
        const cachedSessionID = cached ? sessionFromSnapshot(cached) : undefined;
        if (cached && cachedSessionID === sessionID) {
          return Effect.succeed({ success: true, ...cached });
        }
        return Effect.tryPromise({
          try: (signal) =>
            bridge.send("status", { session_id: sessionID }, { abortSignal: signal }),
          catch: (error) => error,
        }).pipe(
          Effect.tap((response) =>
            Effect.sync(() => {
              if (response.success !== false) bridge.cacheStatusSnapshot(response);
            }),
          ),
        );
      });

    const ownsSession = async (sessionID: string): Promise<boolean> => {
      if (
        typeof context.session?.get !== "function" ||
        context.location?.directory !== root ||
        context.location.workspaceID !== location.workspaceID
      ) {
        return false;
      }
      return (await getSessionDirectoryForEvent(context, sessionID, root)) === root;
    };

    // Only the bound session's active task IDs may reach the status and kill
    // commands; those commands also support broader task lookups for agents.
    const ownedTasks = async (sessionID: string, signal: AbortSignal) => {
      if (!toolContext || !(await ownsSession(sessionID))) {
        throw new Error("AFT background task session could not be verified");
      }
      const runtime = {
        sessionID,
        directory: root,
        directoryIsSessionRoot: true,
        effectAbort: signal,
      };
      const { success, task_ids: ids } = await callBashBridge(
        toolContext,
        runtime,
        "bash_owned_tasks",
      );
      if (
        success !== true ||
        !Array.isArray(ids) ||
        !ids.every((id): id is string => !!nonEmptyString(id))
      ) {
        throw new Error("bash_owned_tasks did not return active task IDs");
      }
      const status = (id: string) =>
        callBashBridge(toolContext, { ...runtime, observeBashStatus: true }, "bash_status", {
          task_id: id,
        });
      return { ids, runtime, status, context: toolContext };
    };

    registration = yield* context.rpc.register(AftRpc, {
      getStatus,
      getBackgroundTasks: ({ sessionID }) =>
        Effect.tryPromise(async (signal) => {
          const { ids, status } = await ownedTasks(sessionID, signal);
          const tasks = await Promise.all(
            ids.map(async (id): Promise<AftBackgroundTask | undefined> => {
              const reply = await status(id);
              if (!taskSnapshot(reply, id, "bash_status")?.active) return;
              const command = nonEmptyString(reply.command);
              const workdir = nonEmptyString(reply.workdir);
              const elapsedMs = finiteNumber(reply.elapsed_ms);
              return {
                id,
                ...(command === undefined ? {} : { command }),
                ...(workdir === undefined ? {} : { workdir }),
                ...(elapsedMs === undefined || elapsedMs < 0 ? {} : { elapsedMs }),
              };
            }),
          );
          return tasks.filter((task) => task !== undefined);
        }),
      stopBackgroundTask: ({ sessionID, taskID }) =>
        Effect.tryPromise(async (signal): Promise<AftStopBackgroundTaskResult> => {
          const { ids, runtime, status, context } = await ownedTasks(sessionID, signal);
          if (!ids.includes(taskID)) return { outcome: "not-active" };
          const before = taskSnapshot(await status(taskID), taskID, "bash_status");
          if (!before) return { outcome: "not-active" };
          if (before.status === "fate_unknown") {
            throw new Error("Background task outcome is unknown");
          }
          if (!before.active) return { outcome: "finished" };

          const reply = await callBashBridge(context, runtime, "bash_kill", { task_id: taskID });
          const after = taskSnapshot(reply, taskID, "bash_kill");
          if (!after) return { outcome: "not-active" };
          // A pending remote cancel isn't confirmation, and a natural exit can
          // win the race with the kill. Use the terminal result, not `success`.
          if (after.active || after.status === "fate_unknown") {
            throw new Error("Background task stop was not confirmed");
          }
          return { outcome: after.status === "killed" ? "stopped" : "finished" };
        }),
    });

    const ownGetBridge = Object.getOwnPropertyDescriptor(pool, "getBridge");
    const ownToolCall = Object.getOwnPropertyDescriptor(pool, "toolCall");
    const getBridge = pool.getBridge;
    const toolCall = pool.toolCall;
    Object.defineProperty(pool, "getBridge", {
      configurable: true,
      writable: true,
      value(projectRoot: string) {
        const bridge = getBridge.call(pool, projectRoot);
        observeBridge(bridge);
        return bridge;
      },
    });
    Object.defineProperty(pool, "toolCall", {
      configurable: true,
      writable: true,
      async value(...args: Parameters<AftTransportPool["toolCall"]>) {
        try {
          return await toolCall.apply(pool, args);
        } finally {
          observeBridge(pool.getActiveBridgeForRoot(args[0]));
        }
      },
    });
    observeBridge(pool.getActiveBridgeForRoot(root));

    const removeNotificationSink = registerNotificationSink({
      send(notification: RpcNotification) {
        if (notification.type !== "action") return;
        if (notification.payload.action !== "show-status-dialog") return;
        const sessionID = notification.sessionId ?? nonEmptyString(notification.payload.sessionId);
        void emit("showStatusDialog", sessionID ? { sessionID } : {}).catch(() => {});
      },
    });
    const forwardStatusChange = async (sessionID: string | undefined): Promise<void> => {
      // The compatibility bus is process-wide. Only sessionless invalidations
      // belong to every Location; a tagged one needs host ownership proof.
      if (sessionID === undefined) await emit("statusInvalidated", {});
      else if (await ownsSession(sessionID)) await emit("statusInvalidated", { sessionID });
    };
    const removeStatusChangeSink = registerStatusChangeSink({
      send({ sessionId }) {
        void forwardStatusChange(sessionId).catch(() => {});
      },
    });

    return {
      emitStatusInvalidated: (payload = {}) => emit("statusInvalidated", payload),
      emitShowStatusDialog: (payload = {}) => emit("showStatusDialog", payload),
      emitIndexProgress: (payload) => emit("indexProgress", payload),
      emitHostToolOverlap: (payload) => emit("hostToolOverlap", payload),
      async dispose() {
        if (disposed) return;
        disposed = true;
        removeNotificationSink();
        removeStatusChangeSink();
        for (const unsubscribe of bridgeUnsubscribes.values()) unsubscribe();
        bridgeUnsubscribes.clear();
        restoreOwnProperty(pool, "getBridge", ownGetBridge);
        restoreOwnProperty(pool, "toolCall", ownToolCall);
        await Effect.runPromise(registration.dispose);
      },
    };
  });
}

/**
 * Status endpoint for the config error state. There is no bridge to observe,
 * so every status request answers with the configuration error, which the
 * sidebar and footer render as one line.
 */
export function registerAftConfigErrorRpc(
  context: AftRpcContext,
  message: string,
): Effect.Effect<RegisteredAftRpc, unknown, Scope.Scope> {
  return Effect.gen(function* () {
    let disposed = false;
    const registration = yield* context.rpc.register(AftRpc, {
      getStatus: () => Effect.succeed(configErrorStatusSnapshot(message)),
      getBackgroundTasks: () => Effect.fail(new Error("AFT background tasks unavailable")),
      stopBackgroundTask: () => Effect.fail(new Error("AFT background tasks unavailable")),
    });
    const emit = async (
      name: keyof typeof AftRpc.events,
      payload: Record<string, unknown>,
    ): Promise<void> => {
      if (disposed) return;
      await Effect.runPromise(registration.events.emit(name, payload));
    };
    return {
      emitStatusInvalidated: (payload = {}) => emit("statusInvalidated", payload),
      emitShowStatusDialog: (payload = {}) => emit("showStatusDialog", payload),
      emitIndexProgress: (payload) => emit("indexProgress", payload),
      emitHostToolOverlap: (payload) => emit("hostToolOverlap", payload),
      async dispose() {
        if (disposed) return;
        disposed = true;
        await Effect.runPromise(registration.dispose);
      },
    };
  });
}
