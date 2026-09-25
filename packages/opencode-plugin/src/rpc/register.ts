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
import { NOT_STARTED_STATUS_TEXT } from "../shared/status";
import { type AftIndexProgress, AftRpc, type AftRpcSession } from "./contract";

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
  rpc: {
    register(
      definition: typeof AftRpc,
      handlers: {
        getStatus(input: AftRpcSession): Effect.Effect<Record<string, unknown>, unknown>;
      },
    ): Effect.Effect<RpcRegistration, unknown, Scope.Scope>;
  };
};

export type AftRpcLocation = {
  directory: string;
  project?: {
    directory?: string;
    canonical?: string;
  };
};

export type RegisteredAftRpc = {
  emitStatusInvalidated(payload?: AftRpcSession): Promise<void>;
  emitShowStatusDialog(payload?: AftRpcSession): Promise<void>;
  emitIndexProgress(payload: AftIndexProgress): Promise<void>;
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

function locationRoot(location: AftRpcLocation): string {
  return location.project?.canonical ?? location.project?.directory ?? location.directory;
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

export function registerAftRpc(
  context: AftRpcContext,
  location: AftRpcLocation,
  pool: AftTransportPool,
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

    registration = yield* context.rpc.register(AftRpc, { getStatus });

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
    const removeStatusChangeSink = registerStatusChangeSink({
      send(event) {
        void emit("statusInvalidated", event.sessionId ? { sessionID: event.sessionId } : {}).catch(
          () => {},
        );
      },
    });

    return {
      emitStatusInvalidated: (payload = {}) => emit("statusInvalidated", payload),
      emitShowStatusDialog: (payload = {}) => emit("showStatusDialog", payload),
      emitIndexProgress: (payload) => emit("indexProgress", payload),
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
      async dispose() {
        if (disposed) return;
        disposed = true;
        await Effect.runPromise(registration.dispose);
      },
    };
  });
}
