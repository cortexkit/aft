/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { __recordRpcRedirectForTest, __resetRpcRedirectsForTest } from "../shared/rpc-client.js";
import {
  __resetAftTuiSocketForTest,
  __setAftTuiSocketDepsForTest,
  createDebouncedStatusRefresh,
  startAftTuiSocket,
  stopAftTuiSocket,
  subscribeStatusInvalidations,
} from "../tui/notification-socket.js";

type Listener = (event: unknown) => void;

class FakeWebSocket {
  static instances: FakeWebSocket[] = [];
  readyState = 0;
  sent: string[] = [];
  private listeners = new Map<string, Listener[]>();

  constructor(readonly url: string) {
    FakeWebSocket.instances.push(this);
  }

  addEventListener(type: "open" | "message" | "close" | "error", listener: Listener): void {
    const list = this.listeners.get(type) ?? [];
    list.push(listener);
    this.listeners.set(type, list);
  }

  send(data: string): void {
    this.sent.push(data);
  }

  close(): void {
    if (this.readyState === 3) return;
    this.readyState = 3;
    this.emit("close", {});
  }

  open(): void {
    this.readyState = 1;
    this.emit("open", {});
  }

  message(data: string): void {
    this.emit("message", { data });
  }

  private emit(type: string, event: unknown): void {
    for (const listener of this.listeners.get(type) ?? []) listener(event);
  }
}

const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

beforeEach(() => {
  FakeWebSocket.instances = [];
  __resetAftTuiSocketForTest();
  __resetRpcRedirectsForTest();
});

afterEach(() => {
  stopAftTuiSocket();
  __resetAftTuiSocketForTest();
  __resetRpcRedirectsForTest();
});

describe("TUI notification socket", () => {
  test("generation guard abandons a stale in-flight connect", async () => {
    let resolveEndpoint: (endpoint: { port: number; token: string }) => void = () => {};
    const endpointPromise = new Promise<{ port: number; token: string }>((resolve) => {
      resolveEndpoint = resolve;
    });

    __setAftTuiSocketDepsForTest({
      createClient: () => ({
        resolveEndpoint: () => endpointPromise,
        reset: () => {},
      }),
      WebSocketCtor: FakeWebSocket as any,
    });

    startAftTuiSocket({
      getDirectory: () => "/project",
      getSessionId: () => "ses_A",
      onNotification: () => true,
    });
    stopAftTuiSocket();
    resolveEndpoint({ port: 1234, token: "token" });
    await flush();

    expect(FakeWebSocket.instances).toHaveLength(0);
  });

  test("learned verified-directory redirect re-homes a connected socket", async () => {
    // The socket is connected to the TUI directory's own (bridgeless)
    // instance. When a status call learns that the session's bridge lives in
    // another directory's process, the socket must drop and reconnect so it
    // subscribes to the instance that will actually push status-changed
    // frames — staying on the bridgeless instance means silence forever.
    const endpoints = new Map<string, { port: number; token: string }>([
      ["/home", { port: 1111, token: "home-token" }],
      ["/project", { port: 2222, token: "project-token" }],
    ]);
    const resolvedFor: string[] = [];

    __setAftTuiSocketDepsForTest({
      createClient: (directory: string) =>
        ({
          resolveEndpoint: async () => {
            resolvedFor.push(directory);
            return endpoints.get(directory) ?? null;
          },
          reset: () => {},
        }) as any,
      WebSocketCtor: FakeWebSocket as any,
    });

    startAftTuiSocket({
      getDirectory: () => "/home",
      getSessionId: () => "ses_A",
      onNotification: () => true,
    });
    await flush();

    expect(FakeWebSocket.instances).toHaveLength(1);
    const first = FakeWebSocket.instances[0] as FakeWebSocket;
    expect(first.url).toContain(":1111");
    first.open();

    // A status fetch (sidebar refresh) learns the redirect.
    __recordRpcRedirectForTest("/home", "/project");
    await flush();

    // Old socket dropped, new one dialed. The fake createClient stands in for
    // the real resolveEndpoint redirect-following, so the reconnect resolving
    // through /home again is fine — the assertion is drop-and-reconnect.
    expect(first.readyState).toBe(3);
    expect(FakeWebSocket.instances.length).toBe(2);
  });

  test("status-change pushes coalesce into one debounced status fetch", async () => {
    __setAftTuiSocketDepsForTest({
      createClient: () => ({
        resolveEndpoint: async () => ({ port: 7777, token: "secret" }),
        reset: () => {},
      }),
      WebSocketCtor: FakeWebSocket as any,
    });

    let fetches = 0;
    const debouncer = createDebouncedStatusRefresh(() => {
      fetches += 1;
    }, 5);
    const unsubscribe = subscribeStatusInvalidations((event) => {
      if (event.sessionId && event.sessionId !== "ses_A") return;
      debouncer.schedule();
    });

    startAftTuiSocket({
      getDirectory: () => "/project",
      getSessionId: () => "ses_A",
      onNotification: () => true,
    });
    await flush();
    const ws = FakeWebSocket.instances[0]!;
    ws.open();

    for (let i = 0; i < 5; i += 1) {
      ws.message(JSON.stringify({ type: "status-changed", sessionId: "ses_A" }));
    }
    ws.message(JSON.stringify({ type: "status-changed", sessionId: "ses_B" }));

    await new Promise((resolve) => setTimeout(resolve, 25));
    unsubscribe();
    debouncer.dispose();

    expect(fetches).toBe(1);
  });

  test("handled notifications are acked and ride the next hello cursor", async () => {
    __setAftTuiSocketDepsForTest({
      createClient: () => ({
        resolveEndpoint: async () => ({ port: 7777, token: "secret" }),
        reset: () => {},
      }),
      WebSocketCtor: FakeWebSocket as any,
    });

    startAftTuiSocket({
      getDirectory: () => "/project",
      getSessionId: () => "ses_A",
      onNotification: () => true,
    });
    await flush();
    const first = FakeWebSocket.instances[0]!;
    first.open();
    first.message(
      JSON.stringify({
        type: "notification",
        notification: { id: 41, type: "action", payload: {}, sessionId: "ses_A" },
      }),
    );
    await flush();

    expect(first.sent.map((message) => JSON.parse(message))).toContainEqual({
      type: "ack",
      lastReceivedId: 41,
    });

    first.close();
    await new Promise((resolve) => setTimeout(resolve, 550));
    await flush();
    const second = FakeWebSocket.instances[1]!;
    second.open();

    expect(JSON.parse(second.sent[0]!)).toMatchObject({
      type: "hello",
      sessionId: "ses_A",
      lastReceivedId: 41,
    });
  });

  test("TUI status and notification polling intervals are absent", () => {
    const root = join(import.meta.dir, "..");
    const index = readFileSync(join(root, "tui", "index.tsx"), "utf-8");
    const sidebar = readFileSync(join(root, "tui", "sidebar.tsx"), "utf-8");

    expect(index).not.toContain("POLL_INTERVAL_MS");
    expect(sidebar).not.toContain("POLL_INTERVAL_MS");
    expect(index).not.toMatch(/setInterval\s*\(/);
    expect(sidebar).not.toMatch(/setInterval\s*\(/);
  });

  test("notification cursor resets for a new server token even at the same port", async () => {
    let token = "old-incarnation";
    __setAftTuiSocketDepsForTest({
      createClient: () => ({
        resolveEndpoint: async () => ({ port: 7777, token }),
        reset: () => {},
      }),
      WebSocketCtor: FakeWebSocket as any,
    });
    startAftTuiSocket({
      getDirectory: () => "/project",
      getSessionId: () => "ses_A",
      onNotification: () => true,
    });
    await flush();
    const first = FakeWebSocket.instances[0]!;
    first.open();
    first.message(
      JSON.stringify({
        type: "notification",
        notification: { id: 41, type: "action", payload: {} },
      }),
    );
    await flush();
    token = "new-incarnation";
    __recordRpcRedirectForTest("/project", "/new-project");
    await flush();
    const second = FakeWebSocket.instances[1]!;
    second.open();
    expect(JSON.parse(second.sent[0]!).lastReceivedId).toBe(0);
    second.message(
      JSON.stringify({
        type: "notification",
        notification: { id: 1, type: "action", payload: {} },
      }),
    );
    await flush();
    expect(second.sent.map((message) => JSON.parse(message))).toContainEqual({
      type: "ack",
      lastReceivedId: 1,
    });
  });
});
