/// <reference path="../bun-test.d.ts" />

import { afterEach, describe, expect, test } from "bun:test";
import { setActiveLogger } from "../active-logger.js";
import { __setBinaryFingerprintForTests, type BinaryBridge } from "../bridge.js";
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

let nextTestResponseId = 1;

type PendingForTest = {
  resolve: (value: Record<string, unknown>) => void;
  reject: (error: Error) => void;
  timer: ReturnType<typeof setTimeout>;
  command: string;
};

function deliverBridgeResponse(
  bridge: BinaryBridge,
  command: string,
  payload: Record<string, unknown>,
): Record<string, unknown> {
  const id = `test-${nextTestResponseId++}`;
  let resolved: Record<string, unknown> | undefined;
  const timer = setTimeout(() => {}, 10_000);
  timer.unref();
  const internals = bridge as unknown as {
    pending: Map<string, PendingForTest>;
    processStdoutLine(line: string): void;
  };
  internals.pending.set(id, {
    command,
    timer,
    resolve: (value) => {
      resolved = value;
    },
    reject: (error) => {
      throw error;
    },
  });
  internals.processStdoutLine(JSON.stringify({ id, ...payload }));
  if (!resolved) throw new Error(`test response ${id} was not resolved`);
  return resolved;
}

function markOutstandingBackgroundTask(bridge: BinaryBridge, taskId: string): void {
  deliverBridgeResponse(bridge, "bash", { success: true, task_id: taskId, status: "running" });
}

function seedSpawnedBinaryFingerprint(bridge: BinaryBridge, fingerprint: string): void {
  const internals = bridge as unknown as {
    process: { exitCode: number | null; killed: boolean } | null;
    spawnedBinaryFingerprint: string | null;
    lastBinaryFingerprintCheckAt: number;
  };
  internals.process = { exitCode: null, killed: false };
  internals.spawnedBinaryFingerprint = fingerprint;
  internals.lastBinaryFingerprintCheckAt = 0;
}

function isRetiringForBinaryChange(bridge: BinaryBridge): boolean {
  return (bridge as unknown as { _retiringDueToBinaryChange: boolean })._retiringDueToBinaryChange;
}

function deliverBashCompletion(
  bridge: BinaryBridge,
  taskId: string,
  status: "completed" | "failed" | "killed" | "timed_out" = "completed",
): void {
  (bridge as unknown as { processStdoutLine(line: string): void }).processStdoutLine(
    JSON.stringify({
      type: "bash_completed",
      task_id: taskId,
      session_id: "session-1",
      status,
      exit_code: status === "completed" ? 0 : null,
      command: "echo done",
    }),
  );
}

describe("BridgePool lifecycle", () => {
  afterEach(() => {
    __setBinaryFingerprintForTests(null);
  });

  test("forwards bash pattern match handler into created bridges", () => {
    const onBashPatternMatch = () => {};
    const pool = new BridgePool("/fake/aft", { idleTimeoutMs: Infinity, onBashPatternMatch });

    const bridge = pool.getBridge("/project/pattern-match");

    expect((bridge as unknown as { onBashPatternMatch: unknown }).onBashPatternMatch).toBe(
      onBashPatternMatch,
    );
  });

  test("default finite idle timeout starts an unrefed cleanup timer", async () => {
    const pool = new BridgePool("/fake/aft");
    try {
      const timer = (pool as unknown as { cleanupTimer: ReturnType<typeof setInterval> | null })
        .cleanupTimer;
      expect(timer).not.toBeNull();
      expect(typeof timer?.hasRef).toBe("function");
      expect(timer?.hasRef()).toBe(false);
    } finally {
      await pool.shutdown();
    }
  });

  test("cleanup evicts an idle bridge after a small finite timeout", async () => {
    const pool = new BridgePool("/fake/aft", { idleTimeoutMs: 1 });
    try {
      const bridge = pool.getBridge("/project/idle-cleanup");
      let shutdownCalls = 0;
      (bridge as unknown as { shutdown: () => Promise<void> }).shutdown = async () => {
        shutdownCalls += 1;
      };
      const entries = (
        pool as unknown as { bridges: Map<string, { bridge: unknown; lastUsed: number }> }
      ).bridges;
      for (const entry of entries.values()) entry.lastUsed = 0;

      (pool as unknown as { cleanup(): void }).cleanup();

      expect(shutdownCalls).toBe(1);
      expect(pool.size).toBe(0);
    } finally {
      await pool.shutdown();
    }
  });

  test("cleanup skips outstanding background tasks until completion and idle timeout", async () => {
    const pool = new BridgePool("/fake/aft", { idleTimeoutMs: 10 });
    try {
      const bridge = pool.getBridge("/project/bg-cleanup");
      let shutdownCalls = 0;
      (bridge as unknown as { shutdown: () => Promise<void> }).shutdown = async () => {
        shutdownCalls += 1;
      };
      markOutstandingBackgroundTask(bridge, "bash-bg-cleanup");
      const entries = (
        pool as unknown as { bridges: Map<string, { bridge: BinaryBridge; lastUsed: number }> }
      ).bridges;
      const entry = Array.from(entries.values()).find((candidate) => candidate.bridge === bridge);
      if (!entry) throw new Error("test bridge not found");
      entry.lastUsed = Date.now() - 20;

      (pool as unknown as { cleanup(): void }).cleanup();

      expect(shutdownCalls).toBe(0);
      expect(pool.size).toBe(1);
      expect(bridge.hasOutstandingBackgroundTasks()).toBe(true);

      deliverBashCompletion(bridge, "bash-bg-cleanup");
      expect(bridge.hasOutstandingBackgroundTasks()).toBe(false);
      entry.lastUsed = Date.now();
      (pool as unknown as { cleanup(): void }).cleanup();
      expect(shutdownCalls).toBe(0);
      expect(pool.size).toBe(1);

      entry.lastUsed = Date.now() - 20;
      (pool as unknown as { cleanup(): void }).cleanup();

      expect(shutdownCalls).toBe(1);
      expect(pool.size).toBe(0);
    } finally {
      await pool.shutdown();
    }
  });

  test("replaceBinary keeps old bridges reachable for cleanup and shutdown", async () => {
    const pool = new BridgePool("/fake/old-aft", { idleTimeoutMs: Infinity });
    const bridge = pool.getBridge("/project/stale-bridge");
    let shutdownCalls = 0;
    (bridge as unknown as { shutdown: () => Promise<void> }).shutdown = async () => {
      shutdownCalls += 1;
    };

    await pool.replaceBinary("/fake/new-aft");

    const internals = pool as unknown as {
      bridges: Map<string, unknown>;
      staleBridges: Set<unknown>;
      cleanup(): void;
    };
    expect(internals.bridges.size).toBe(0);
    expect(internals.staleBridges.has(bridge)).toBe(true);

    internals.cleanup();
    await Promise.resolve();
    expect(shutdownCalls).toBe(1);
    expect(internals.staleBridges.size).toBe(0);

    await pool.shutdown();
    expect(shutdownCalls).toBe(1);
  });

  test("shutdown drains pending stale bridges left by replaceBinary", async () => {
    const pool = new BridgePool("/fake/old-aft", { idleTimeoutMs: Infinity });
    const bridge = pool.getBridge("/project/pending-stale-bridge");
    let shutdownCalls = 0;
    (
      bridge as unknown as {
        pending: Map<string, unknown>;
        shutdown: () => Promise<void>;
      }
    ).pending.set("1", {});
    (bridge as unknown as { shutdown: () => Promise<void> }).shutdown = async () => {
      shutdownCalls += 1;
    };

    await pool.replaceBinary("/fake/new-aft");

    const internals = pool as unknown as {
      staleBridges: Set<unknown>;
      cleanup(): void;
    };
    internals.cleanup();
    await Promise.resolve();
    expect(shutdownCalls).toBe(0);
    expect(internals.staleBridges.has(bridge)).toBe(true);

    await pool.shutdown();
    expect(shutdownCalls).toBe(1);
    expect(internals.staleBridges.size).toBe(0);
  });

  test("cleanup skips stale bridges with outstanding background tasks", async () => {
    const pool = new BridgePool("/fake/old-aft", { idleTimeoutMs: Infinity });
    const bridge = pool.getBridge("/project/background-stale-bridge");
    let shutdownCalls = 0;
    markOutstandingBackgroundTask(bridge, "bash-stale");
    (bridge as unknown as { shutdown: () => Promise<void> }).shutdown = async () => {
      shutdownCalls += 1;
    };

    await pool.replaceBinary("/fake/new-aft");

    const internals = pool as unknown as {
      staleBridges: Set<unknown>;
      cleanup(): void;
    };
    internals.cleanup();
    await Promise.resolve();
    expect(shutdownCalls).toBe(0);
    expect(internals.staleBridges.has(bridge)).toBe(true);

    deliverBashCompletion(bridge, "bash-stale");
    internals.cleanup();
    await Promise.resolve();

    expect(shutdownCalls).toBe(1);
    expect(internals.staleBridges.size).toBe(0);
    await pool.shutdown();
  });

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

  test("LRU eviction skips outstanding background tasks and evicts next candidate", () => {
    const pool = new BridgePool("/fake/aft", { idleTimeoutMs: Infinity, maxPoolSize: 2 });
    const first = pool.getBridge("/project/bg-lru-first");
    const second = pool.getBridge("/project/bg-lru-second");
    markOutstandingBackgroundTask(first, "bash-bg-lru");
    let firstShutdowns = 0;
    let secondShutdowns = 0;
    (first as unknown as { shutdown: () => Promise<void> }).shutdown = async () => {
      firstShutdowns += 1;
    };
    (second as unknown as { shutdown: () => Promise<void> }).shutdown = async () => {
      secondShutdowns += 1;
    };

    const entries = (
      pool as unknown as { bridges: Map<string, { bridge: BinaryBridge; lastUsed: number }> }
    ).bridges;
    const firstEntry = Array.from(entries.values()).find((entry) => entry.bridge === first);
    const secondEntry = Array.from(entries.values()).find((entry) => entry.bridge === second);
    if (!firstEntry || !secondEntry) throw new Error("test bridges not found");
    firstEntry.lastUsed = 1;
    secondEntry.lastUsed = 2;

    const third = pool.getBridge("/project/bg-lru-third");

    expect(firstShutdowns).toBe(0);
    expect(secondShutdowns).toBe(1);
    expect(Array.from(entries.values()).some((entry) => entry.bridge === first)).toBe(true);
    expect(Array.from(entries.values()).some((entry) => entry.bridge === second)).toBe(false);
    expect(Array.from(entries.values()).some((entry) => entry.bridge === third)).toBe(true);
    expect(pool.size).toBe(2);
  });

  test("LRU eviction evicts none when every bridge is pending or has background tasks", () => {
    const pool = new BridgePool("/fake/aft", { idleTimeoutMs: Infinity, maxPoolSize: 2 });
    const pending = pool.getBridge("/project/all-busy-pending");
    const background = pool.getBridge("/project/all-busy-background");
    (pending as unknown as { pending: Map<string, unknown> }).pending.set("1", {});
    markOutstandingBackgroundTask(background, "bash-all-busy");
    let shutdownCalls = 0;
    for (const bridge of [pending, background]) {
      (bridge as unknown as { shutdown: () => Promise<void> }).shutdown = async () => {
        shutdownCalls += 1;
      };
    }

    pool.getBridge("/project/all-busy-new");

    const entries = (
      pool as unknown as { bridges: Map<string, { bridge: unknown; lastUsed: number }> }
    ).bridges;
    expect(shutdownCalls).toBe(0);
    expect(Array.from(entries.values()).some((entry) => entry.bridge === pending)).toBe(true);
    expect(Array.from(entries.values()).some((entry) => entry.bridge === background)).toBe(true);
    expect(pool.size).toBe(3);
  });

  test("cleanup retires a bridge when the on-disk binary hash changes", async () => {
    const pool = new BridgePool("/fake/aft", { idleTimeoutMs: Infinity });
    const bridge = pool.getBridge("/project/hash-change");
    seedSpawnedBinaryFingerprint(bridge, "spawned-hash");
    let fingerprintReads = 0;
    let shutdownCalls = 0;
    __setBinaryFingerprintForTests(() => {
      fingerprintReads += 1;
      return "updated-hash";
    });
    (bridge as unknown as { shutdown: () => Promise<void> }).shutdown = async () => {
      shutdownCalls += 1;
    };

    (pool as unknown as { cleanup(): void }).cleanup();
    await Promise.resolve();

    expect(fingerprintReads).toBe(1);
    expect(shutdownCalls).toBe(1);
    expect(isRetiringForBinaryChange(bridge)).toBe(true);
    expect(pool.size).toBe(0);
  });

  test("cleanup leaves the bridge alone when the on-disk binary hash is unchanged", () => {
    const pool = new BridgePool("/fake/aft", { idleTimeoutMs: Infinity });
    const bridge = pool.getBridge("/project/hash-same");
    seedSpawnedBinaryFingerprint(bridge, "same-hash");
    let fingerprintReads = 0;
    let shutdownCalls = 0;
    __setBinaryFingerprintForTests(() => {
      fingerprintReads += 1;
      return "same-hash";
    });
    (bridge as unknown as { shutdown: () => Promise<void> }).shutdown = async () => {
      shutdownCalls += 1;
    };

    (pool as unknown as { cleanup(): void }).cleanup();

    expect(fingerprintReads).toBe(1);
    expect(shutdownCalls).toBe(0);
    expect(isRetiringForBinaryChange(bridge)).toBe(false);
    expect(pool.size).toBe(1);
  });

  test("cleanup ignores unreadable binaries while a rebuild is in flight", () => {
    const pool = new BridgePool("/fake/aft", { idleTimeoutMs: Infinity });
    const bridge = pool.getBridge("/project/hash-unreadable");
    seedSpawnedBinaryFingerprint(bridge, "spawned-hash");
    let shutdownCalls = 0;
    __setBinaryFingerprintForTests(() => null);
    (bridge as unknown as { shutdown: () => Promise<void> }).shutdown = async () => {
      shutdownCalls += 1;
    };

    (pool as unknown as { cleanup(): void }).cleanup();

    expect(shutdownCalls).toBe(0);
    expect(isRetiringForBinaryChange(bridge)).toBe(false);
    expect(pool.size).toBe(1);
  });

  test("cleanup defers binary refresh checks while requests are still in flight", async () => {
    const pool = new BridgePool("/fake/aft", { idleTimeoutMs: Infinity });
    const bridge = pool.getBridge("/project/hash-pending");
    seedSpawnedBinaryFingerprint(bridge, "spawned-hash");
    let fingerprintReads = 0;
    let shutdownCalls = 0;
    __setBinaryFingerprintForTests(() => {
      fingerprintReads += 1;
      return "updated-hash";
    });
    (bridge as unknown as { shutdown: () => Promise<void> }).shutdown = async () => {
      shutdownCalls += 1;
    };
    const pending = (bridge as unknown as { pending: Map<string, unknown> }).pending;
    pending.set("1", {});

    (pool as unknown as { cleanup(): void }).cleanup();

    expect(fingerprintReads).toBe(0);
    expect(shutdownCalls).toBe(0);
    expect(pool.size).toBe(1);

    pending.clear();
    (pool as unknown as { cleanup(): void }).cleanup();
    await Promise.resolve();

    expect(fingerprintReads).toBe(1);
    expect(shutdownCalls).toBe(1);
    expect(isRetiringForBinaryChange(bridge)).toBe(true);
    expect(pool.size).toBe(0);
  });

  test("constructor logger handles pool logs instead of active singleton", async () => {
    const custom = makeLogger();
    const active = makeLogger();
    setActiveLogger(active.logger);

    const pool = new BridgePool("/fake/aft", { idleTimeoutMs: 1, logger: custom.logger });
    const rejectingBridge = {
      hasPendingRequests: () => false,
      hasOutstandingBackgroundTasks: () => false,
      maybeScheduleRespawnForUpdatedBinary: () => false,
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
