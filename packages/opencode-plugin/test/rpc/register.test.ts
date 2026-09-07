import { afterEach, describe, expect, test } from "bun:test";

import type { AftTransportPool } from "@cortexkit/aft-bridge";
import { AftRpc } from "../../src/rpc/contract.js";
import {
  type AftRpcContext,
  indexProgressFromStatus,
  registerAftRpc,
} from "../../src/rpc/register.js";
import {
  __resetRpcNotificationsForTest,
  pushNotification,
} from "../../src/shared/rpc-notifications.js";

type RpcEvent = { name: string; payload: Record<string, unknown> };
type GetStatus = (input: { sessionID?: string }) => Promise<Record<string, unknown>>;

class StatusBridge {
  snapshot: Record<string, unknown> | null;
  sends: Array<{
    command: string;
    params: Record<string, unknown> | undefined;
    options: { abortSignal?: AbortSignal } | undefined;
  }> = [];
  private listeners = new Set<(snapshot: Record<string, unknown>) => void>();

  constructor(snapshot: Record<string, unknown> | null) {
    this.snapshot = snapshot;
  }

  getCwd(): string {
    return "/work/project";
  }

  getCachedStatus(): Record<string, unknown> | null {
    return this.snapshot;
  }

  cacheStatusSnapshot(snapshot: Record<string, unknown>): void {
    this.snapshot = snapshot;
  }

  async send(
    command: string,
    params?: Record<string, unknown>,
    options?: { abortSignal?: AbortSignal },
  ): Promise<Record<string, unknown>> {
    this.sends.push({ command, params, options });
    return this.snapshot ?? { success: true, status: "ready" };
  }

  async toolCall(): Promise<never> {
    throw new Error("unused");
  }

  subscribeStatus(listener: (snapshot: Record<string, unknown>) => void): () => void {
    this.listeners.add(listener);
    if (this.snapshot) listener(this.snapshot);
    return () => this.listeners.delete(listener);
  }

  publish(snapshot: Record<string, unknown>): void {
    this.snapshot = snapshot;
    for (const listener of this.listeners) listener(snapshot);
  }
}

function hostHarness() {
  let getStatus: GetStatus | undefined;
  let registrations = 0;
  let disposals = 0;
  const clients = {
    tui: [] as RpcEvent[],
    desktop: [] as RpcEvent[],
    sdk: [] as RpcEvent[],
  };
  const context: AftRpcContext = {
    rpc: {
      async register(definition, handlers) {
        expect(definition).toBe(AftRpc);
        registrations += 1;
        getStatus = handlers.getStatus;
        return {
          events: {
            async emit(name, payload) {
              for (const events of Object.values(clients)) events.push({ name, payload });
            },
          },
          async dispose() {
            disposals += 1;
          },
        };
      },
    },
  };
  return {
    context,
    clients,
    getStatus: () => {
      if (!getStatus) throw new Error("getStatus was not registered");
      return getStatus;
    },
    registrations: () => registrations,
    disposals: () => disposals,
  };
}

function poolHarness(bridge: StatusBridge | null): AftTransportPool {
  return {
    getBridge: () => {
      if (!bridge) throw new Error("cold bridge should not be created by getStatus");
      return bridge;
    },
    getActiveBridgeForRoot: () => bridge,
    activeBridges: () => (bridge ? [bridge] : []),
    toolCall: async () => ({ success: true, text: "ok" }),
    setConfigureOverride: () => {},
    reconfigure: async () => {},
    replaceBinary: async (path) => path,
    isShutdown: () => false,
    shutdown: async () => {},
    closeSession: async () => {},
  } as AftTransportPool;
}

function statusSnapshot(sessionID = "ses_1"): Record<string, unknown> {
  return {
    success: true,
    session: { id: sessionID },
    search_index: { status: "ready", files: 12 },
    semantic_index: {
      status: "loading",
      stage: "embedding",
      entries_done: 4,
      entries_total: 10,
    },
  };
}

async function settleEmits(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
}

afterEach(() => __resetRpcNotificationsForTest());

describe("registerAftRpc", () => {
  test("keeps cold status reads lazy and round-trips warm status", async () => {
    const coldHost = hostHarness();
    const cold = await registerAftRpc(
      coldHost.context,
      { directory: "/work/project" },
      poolHarness(null),
    );
    expect(await coldHost.getStatus()({ sessionID: "ses_cold" })).toMatchObject({
      success: true,
      cache_role: "not_initialized",
    });
    await cold.dispose();

    const bridge = new StatusBridge(statusSnapshot());
    const warmHost = hostHarness();
    const warm = await registerAftRpc(
      warmHost.context,
      { directory: "/work/project" },
      poolHarness(bridge),
    );
    expect(await warmHost.getStatus()({ sessionID: "ses_1" })).toMatchObject({
      success: true,
      session: { id: "ses_1" },
    });
    expect(bridge.sends).toEqual([]);
    await warm.dispose();
  });

  test("threads RPC cancellation into an uncached status request", async () => {
    const bridge = new StatusBridge(statusSnapshot("ses_other"));
    const host = hostHarness();
    const registered = await registerAftRpc(
      host.context,
      { directory: "/work/project" },
      poolHarness(bridge),
    );
    const controller = new AbortController();
    const handler = host.getStatus() as (
      input: { sessionID?: string },
      context: { signal: AbortSignal },
    ) => Promise<Record<string, unknown>>;

    await handler({ sessionID: "ses_requested" }, { signal: controller.signal });

    expect(bridge.sends).toEqual([
      {
        command: "status",
        params: { session_id: "ses_requested" },
        options: { abortSignal: controller.signal },
      },
    ]);
    await registered.dispose();
  });

  test("fans index progress to TUI, Desktop, and headless SDK clients", async () => {
    const bridge = new StatusBridge(null);
    const host = hostHarness();
    const registered = await registerAftRpc(
      host.context,
      { directory: "/work/project" },
      poolHarness(bridge),
    );

    bridge.publish(statusSnapshot("ses_progress"));
    await host.getStatus()({ sessionID: "ses_progress" });
    await settleEmits();

    for (const events of Object.values(host.clients)) {
      expect(events).toContainEqual({
        name: "indexProgress",
        payload: {
          index: "semantic",
          status: "loading",
          sessionID: "ses_progress",
          stage: "embedding",
          completed: 4,
          total: 10,
        },
      });
    }
    await registered.dispose();
  });

  test("maps the private dialog notification onto the typed event without a socket", async () => {
    const host = hostHarness();
    const registered = await registerAftRpc(
      host.context,
      { directory: "/work/project" },
      poolHarness(null),
    );

    pushNotification(
      "action",
      { action: "show-status-dialog", sessionId: "ses_dialog" },
      "ses_dialog",
    );
    await settleEmits();
    expect(host.clients.tui).toContainEqual({
      name: "showStatusDialog",
      payload: { sessionID: "ses_dialog" },
    });
    await registered.dispose();
  });

  test("re-registers after Location reload and disposes each supervisor registration", async () => {
    const host = hostHarness();
    const first = await registerAftRpc(
      host.context,
      { directory: "/work/project" },
      poolHarness(null),
    );
    await first.emitIndexProgress({ index: "search", status: "building", completed: 1 });
    await first.dispose();

    const reloaded = await registerAftRpc(
      host.context,
      { directory: "/work/project" },
      poolHarness(null),
    );
    await reloaded.emitIndexProgress({ index: "search", status: "ready", completed: 12 });
    await reloaded.dispose();

    expect(host.registrations()).toBe(2);
    expect(host.disposals()).toBe(2);
    expect(host.clients.sdk.filter(({ name }) => name === "indexProgress")).toEqual([
      { name: "indexProgress", payload: { index: "search", status: "building", completed: 1 } },
      { name: "indexProgress", payload: { index: "search", status: "ready", completed: 12 } },
    ]);
  });

  test("extracts progress payloads from status snapshots", () => {
    expect(indexProgressFromStatus(statusSnapshot("ses_extract"))).toEqual([
      { index: "search", status: "ready", sessionID: "ses_extract", completed: 12 },
      {
        index: "semantic",
        status: "loading",
        sessionID: "ses_extract",
        stage: "embedding",
        completed: 4,
        total: 10,
      },
    ]);
  });
});
