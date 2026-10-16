/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  type BindIdentity,
  type RequestOptions,
  type RouteHandle,
  type RouteTarget,
  SubcError,
} from "@cortexkit/subc-client";
import { getActiveLogger, setActiveLogger } from "../active-logger.js";
import type { Logger } from "../logger.js";
import {
  type SubcClientLike,
  type SubcSubscriptionLike,
  SubcTransportPool,
} from "../subc-transport.js";

function envelope(text = "ok"): Record<string, unknown> {
  return {
    content: [{ type: "text", text }],
    isError: false,
    structuredContent: { success: true, text },
  };
}

class FakeSubscription implements SubcSubscriptionLike {
  private rejectClosed!: (error: Error) => void;
  readonly closed: Promise<void>;

  constructor() {
    this.closed = new Promise<void>((_resolve, reject) => {
      this.rejectClosed = reject;
    });
    this.closed.catch(() => undefined);
  }

  drop(): void {
    this.rejectClosed(new Error("subscription dropped"));
  }

  unsubscribe(): void {}
}

class FakeClient implements SubcClientLike {
  readonly routeOpens: BindIdentity[] = [];
  readonly subscriptions: FakeSubscription[] = [];
  nextRouteError: Error | null = null;
  beforeRouteOpen: (() => void) | null = null;
  private nextChannel = 1;

  async routeOpen(_target: RouteTarget, identity: BindIdentity): Promise<RouteHandle> {
    this.routeOpens.push(identity);
    this.beforeRouteOpen?.();
    this.beforeRouteOpen = null;
    if (this.nextRouteError) {
      const error = this.nextRouteError;
      this.nextRouteError = null;
      throw error;
    }
    const channel = this.nextChannel++;
    return { channel, epoch: channel } as RouteHandle;
  }

  async request(_route: RouteHandle, _body: unknown, _options?: RequestOptions): Promise<unknown> {
    return envelope();
  }

  subscribe(
    _route: RouteHandle,
    _body: unknown,
    _onEvent: (event: Uint8Array) => void,
  ): SubcSubscriptionLike {
    const subscription = new FakeSubscription();
    this.subscriptions.push(subscription);
    return subscription;
  }

  async closeRouteChannel(_route: RouteHandle): Promise<void> {}

  close(): void {}
}

function makePool(client: FakeClient, onBgEventsNudge?: () => void): SubcTransportPool {
  return new SubcTransportPool({
    connectionFile: "/tmp/fake-subc-connection.json",
    harness: "opencode",
    connect: async () => client,
    onBgEventsNudge,
    bgBackoffSleep: async () => undefined,
  });
}

describe("SubcTransport root dormancy", () => {
  let root: string;
  let previousLogger: Logger | undefined;

  beforeEach(() => {
    root = mkdtempSync(join(tmpdir(), "aft-root-dormancy-"));
    previousLogger = getActiveLogger();
  });

  afterEach(() => {
    rmSync(root, { recursive: true, force: true });
    rmSync(`${root}.reclaimed`, { force: true });
    const slot = globalThis as Record<symbol, unknown>;
    const key = Symbol.for("aft-bridge-active-logger");
    if (previousLogger) setActiveLogger(previousLogger);
    else delete slot[key];
  });

  test("a nonexistent-root bind rejection suspends further binds, then recovers after recreation", async () => {
    const client = new FakeClient();
    const messages: string[] = [];
    setActiveLogger({
      log: (message) => messages.push(message),
      warn: () => undefined,
      error: () => undefined,
    });
    const pool = makePool(client);
    const bridge = pool.getBridge(root);
    client.nextRouteError = new SubcError(
      `invalid route project root: project root does not exist: ${root}`,
      "config_divergence",
    );

    client.beforeRouteOpen = () => rmSync(root, { recursive: true, force: true });
    await expect(bridge.toolCall("session", "read", {})).rejects.toBeInstanceOf(SubcError);
    expect(client.routeOpens).toHaveLength(1);

    await expect(bridge.toolCall("session", "read", {})).rejects.toMatchObject({
      code: "config_divergence",
    });
    expect(client.routeOpens).toHaveLength(1);

    mkdirSync(root);
    await expect(bridge.toolCall("session", "read", {})).resolves.toMatchObject({
      text: "ok",
    });
    expect(client.routeOpens).toHaveLength(2);
    expect(messages.filter((message) => !message.startsWith("subc head scope:"))).toEqual([
      `root ${bridge.getCwd()} reclaimed/absent; suspending attach until it exists`,
    ]);
  });

  test("a stale reclaim marker never blocks an existing directory", async () => {
    writeFileSync(`${root}.reclaimed`, "not JSON and intentionally unread");
    const client = new FakeClient();

    await expect(
      makePool(client).getBridge(root).toolCall("session", "read", {}),
    ).resolves.toMatchObject({
      text: "ok",
    });
    expect(client.routeOpens).toHaveLength(1);
  });

  test("a reclaim marker beside an absent directory prevents any bind", async () => {
    writeFileSync(`${root}.reclaimed`, "{}");
    rmSync(root, { recursive: true, force: true });
    const client = new FakeClient();

    await expect(
      makePool(client).getBridge(root).toolCall("session", "read", {}),
    ).rejects.toMatchObject({
      code: "config_divergence",
      message: expect.stringContaining("project root does not exist"),
    });
    expect(client.routeOpens).toHaveLength(0);
  });

  test("a different config divergence rejection keeps route retry behavior", async () => {
    const client = new FakeClient();
    const pool = makePool(client);
    client.nextRouteError = new SubcError("configuration differs", "config_divergence");

    await expect(pool.getBridge(root).toolCall("session", "read", {})).rejects.toBeInstanceOf(
      SubcError,
    );
    await expect(pool.getBridge(root).toolCall("session", "read", {})).resolves.toMatchObject({
      text: "ok",
    });
    expect(client.routeOpens).toHaveLength(2);
  });

  test("a dropped bg subscription does not reconnect while its root is absent", async () => {
    const client = new FakeClient();
    const pool = makePool(client, () => undefined);
    await pool.getBridge(root).toolCall("session", "read", {});
    expect(client.routeOpens).toHaveLength(2);

    rmSync(root, { recursive: true, force: true });
    client.subscriptions[0]?.drop();
    await new Promise((resolve) => setTimeout(resolve, 0));
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(client.routeOpens).toHaveLength(2);
    mkdirSync(root);
    await pool.getBridge(root).toolCall("session", "read", {});
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(client.routeOpens).toHaveLength(3);
  });
});
