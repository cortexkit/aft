/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import type {
  BindIdentity,
  RequestOptions,
  RouteHandle,
  RouteTarget,
} from "@cortexkit/subc-client";
import { getActiveLogger, setActiveLogger } from "../active-logger.js";
import type { Logger } from "../logger.js";
import { RevivableTransportPool } from "../revivable-transport.js";
import {
  type SubcClientLike,
  type SubcSubscriptionLike,
  SubcTransportPool,
} from "../subc-transport.js";
import { TEST_PROJECT_ROOT } from "./subc-test-roots.js";

class FakeClient implements SubcClientLike {
  readonly routeOpens: BindIdentity[] = [];
  closed = 0;
  private nextChannel = 1;

  async routeOpen(_target: RouteTarget, identity: BindIdentity): Promise<RouteHandle> {
    this.routeOpens.push(identity);
    const channel = this.nextChannel++;
    return { channel, epoch: channel } as RouteHandle;
  }

  async request(_route: RouteHandle, _body: unknown, _options?: RequestOptions): Promise<unknown> {
    return {
      structuredContent: {
        success: true,
        text: "revived",
      },
    };
  }

  subscribe(
    _route: RouteHandle,
    _body: unknown,
    _onEvent: (event: Uint8Array) => void,
  ): SubcSubscriptionLike {
    return { unsubscribe: () => undefined };
  }

  async closeRouteChannel(_route: RouteHandle): Promise<void> {}

  close(): void {
    this.closed += 1;
  }
}

function makeSubcPool(client: FakeClient): SubcTransportPool {
  return new SubcTransportPool({
    connectionFile: "/tmp/fake-subc-connection.json",
    harness: "opencode",
    connect: async () => client,
  });
}

describe("RevivableTransportPool", () => {
  let previousLogger: Logger | undefined;

  beforeEach(() => {
    previousLogger = getActiveLogger();
  });

  afterEach(() => {
    const slot = globalThis as Record<symbol, unknown>;
    const key = Symbol.for("aft-bridge-active-logger");
    if (previousLogger) setActiveLogger(previousLogger);
    else delete slot[key];
  });

  test("revives a shut-down pool with fresh routes and repeats after the replacement shuts down", async () => {
    const initialClient = new FakeClient();
    const revivedClient = new FakeClient();
    const clients = [initialClient, revivedClient];
    let created = 0;
    const initialPool = makeSubcPool(initialClient);
    const owner = new RevivableTransportPool(initialPool, async () => {
      const client = clients[++created];
      if (!client) throw new Error("unexpected extra pool creation");
      return makeSubcPool(client);
    });
    const transport = owner.getBridge(TEST_PROJECT_ROOT);

    await transport.toolCall("before-shutdown", "read", {});
    await owner.shutdown();
    expect(owner.isShutdown()).toBe(true);
    expect(initialClient.closed).toBe(1);

    const revived = await transport.toolCall("after-shutdown", "read", {});
    expect(revived.text).toBe("revived");
    expect(created).toBe(1);
    expect(initialClient.routeOpens.map((identity) => identity.session)).toEqual([
      "before-shutdown",
    ]);
    expect(revivedClient.routeOpens.map((identity) => identity.session)).toEqual([
      "after-shutdown",
    ]);

    await owner.shutdown();
    expect(owner.isShutdown()).toBe(true);
    expect(revivedClient.closed).toBe(1);
  });

  test("captures edit-slot registration once for subc-backed plugin pools", () => {
    const initialPool = makeSubcPool(new FakeClient());
    const owner = new RevivableTransportPool(initialPool, async () =>
      makeSubcPool(new FakeClient()),
    );

    owner.setConfigureOverride("edit_slot_survives", true);

    expect(initialPool.getEditSlotSurvives()).toBe(true);
    expect(() => owner.setConfigureOverride("edit_slot_survives", false)).toThrow(
      "edit_slot_survives is write-once",
    );
  });

  test("concurrent demand during revival shares one replacement instance", async () => {
    const initialClient = new FakeClient();
    const revivedClient = new FakeClient();
    let created = 0;
    let releaseRevival!: () => void;
    const revivalGate = new Promise<void>((resolve) => {
      releaseRevival = resolve;
    });
    const owner = new RevivableTransportPool(makeSubcPool(initialClient), async () => {
      created += 1;
      await revivalGate;
      return makeSubcPool(revivedClient);
    });
    const transport = owner.getBridge(TEST_PROJECT_ROOT);

    await owner.shutdown();
    const first = transport.toolCall("session-a", "read", {});
    const second = transport.toolCall("session-b", "read", {});
    await Promise.resolve();
    expect(created).toBe(1);
    releaseRevival();

    await Promise.all([first, second]);
    expect(created).toBe(1);
    expect(revivedClient.routeOpens.map((identity) => identity.session)).toEqual([
      "session-a",
      "session-b",
    ]);
  });

  test("parks failed revival attempts so concurrent terminal calls cannot hot-spin", async () => {
    const initialClient = new FakeClient();
    let attempts = 0;
    const owner = new RevivableTransportPool(makeSubcPool(initialClient), () => {
      attempts += 1;
      throw new Error("subc unavailable");
    });
    const transport = owner.getBridge(TEST_PROJECT_ROOT);

    await owner.shutdown();
    await Promise.all(
      Array.from({ length: 20 }, () =>
        transport.toolCall("session", "read", {}).catch(() => undefined),
      ),
    );
    expect(attempts).toBe(1);

    // The failed terminal pool has no immediate retry work. Many arrivals share
    // one delayed retry instead of each recursively starting a fresh connection.
    const queued = Array.from({ length: 20 }, () =>
      transport.toolCall("session", "read", {}).catch(() => undefined),
    );
    await new Promise((resolve) => setTimeout(resolve, 50));
    expect(attempts).toBe(1);
    await Promise.all(queued);
    expect(attempts).toBe(2);
  });

  test("logs the explicit shutdown reason when demand revives the transport", async () => {
    const messages: string[] = [];
    setActiveLogger({
      log: () => undefined,
      warn: (message) => messages.push(message),
      error: () => undefined,
    });
    const initialClient = new FakeClient();
    const revivedClient = new FakeClient();
    const owner = new RevivableTransportPool(makeSubcPool(initialClient), async () =>
      makeSubcPool(revivedClient),
    );

    await owner.shutdown("dispose");
    await owner.getBridge(TEST_PROJECT_ROOT).toolCall("session", "read", {});

    expect(messages).toContain(
      "transport was shut down (reason: dispose) but new demand arrived — reviving",
    );
  });
});
