/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { setActiveLogger } from "../active-logger.js";
import type { Logger } from "../logger.js";
import { BridgePool } from "../pool.js";

function makeLogger() {
  const messages: string[] = [];
  const logger: Logger = {
    log: (message) => messages.push(`log:${message}`),
    warn: (message) => messages.push(`warn:${message}`),
    error: (message) => messages.push(`error:${message}`),
  };
  return { logger, messages };
}

describe("BridgePool lifecycle", () => {
  test("cleanup skips idle bridges with pending requests", () => {
    const pool = new BridgePool("/fake/aft", { idleTimeoutMs: 1 });
    const bridge = pool.getBridge("/project/pending-cleanup");

    (bridge as unknown as { pending: Map<string, unknown> }).pending.set("1", {});
    const entries = (
      pool as unknown as { bridges: Map<string, { bridge: unknown; lastUsed: number }> }
    ).bridges;
    for (const entry of entries.values()) entry.lastUsed = 0;

    (pool as unknown as { cleanup(): void }).cleanup();

    expect(pool.size).toBe(1);
    expect(Array.from(entries.values()).some((entry) => entry.bridge === bridge)).toBe(true);
  });

  test("LRU eviction skips bridges with pending requests", () => {
    const pool = new BridgePool("/fake/aft", { idleTimeoutMs: Infinity, maxPoolSize: 1 });
    const first = pool.getBridge("/project/pending-eviction");
    (first as unknown as { pending: Map<string, unknown> }).pending.set("1", {});

    pool.getBridge("/project/new-entry");

    const entries = (
      pool as unknown as { bridges: Map<string, { bridge: unknown; lastUsed: number }> }
    ).bridges;
    expect(Array.from(entries.values()).some((entry) => entry.bridge === first)).toBe(true);
    expect(pool.size).toBe(2);
  });

  test("constructor logger handles pool logs instead of active singleton", async () => {
    const custom = makeLogger();
    const active = makeLogger();
    setActiveLogger(active.logger);

    const pool = new BridgePool("/fake/aft", { idleTimeoutMs: 1, logger: custom.logger });
    const rejectingBridge = {
      hasPendingRequests: () => false,
      shutdown: () => Promise.reject(new Error("boom")),
    };
    (
      pool as unknown as { bridges: Map<string, { bridge: unknown; lastUsed: number }> }
    ).bridges.set("/project/rejecting", { bridge: rejectingBridge, lastUsed: 0 });

    (pool as unknown as { cleanup(): void }).cleanup();
    await Promise.resolve();

    expect(custom.messages.some((message) => message.includes("cleanup shutdown failed"))).toBe(
      true,
    );
    expect(active.messages.some((message) => message.includes("cleanup shutdown failed"))).toBe(
      false,
    );
  });
});
