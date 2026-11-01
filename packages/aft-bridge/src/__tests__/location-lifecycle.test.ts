import { describe, expect, test } from "bun:test";
import { mkdtempSync, realpathSync, rmSync, symlinkSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { BridgeTransportUnavailableError, BridgeTransportUnknownOutcomeError } from "../bridge.js";
import {
  acquireBridge,
  getBridgeLifecycleTopology,
  LOCATION_RELOADED_NOT_SENT,
  releaseBridge,
  sampleBridgeLifecycleCensus,
  withLocationExecution,
} from "../location-lifecycle.js";
import { BridgePool } from "../pool.js";
import type { AftProjectTransport, AftTransportPool } from "../transport.js";
import type { AftTransportFactoryOptions } from "../transport-factory.js";
import { cachedExecutable } from "./test-utils/cached-executable.js";

class FakePool implements AftTransportPool {
  healthResponse: Record<string, unknown> = { success: true };
  readonly bridge = {
    send: async () => this.healthResponse,
    toolCall: async () => ({ success: true, text: "ok" }),
    getCwd: () => "/fixture",
    getCachedStatus: () => null,
    cacheStatusSnapshot: () => {},
  } satisfies AftProjectTransport;
  shutdownCalls = 0;
  toolCallCalls = 0;
  private shutdownState = false;

  getBridge(): AftProjectTransport {
    return this.bridge;
  }

  getActiveBridgeForRoot(): AftProjectTransport | null {
    return this.shutdownState ? null : this.bridge;
  }

  activeBridges(): AftProjectTransport[] {
    return this.shutdownState ? [] : [this.bridge];
  }

  async toolCall() {
    this.toolCallCalls += 1;
    return { success: true, text: "ok" };
  }

  setConfigureOverride(): void {}

  async reconfigure(): Promise<void> {}

  async replaceBinary(path: string): Promise<string> {
    return path;
  }

  isShutdown(): boolean {
    return this.shutdownState;
  }

  async shutdown(): Promise<void> {
    this.shutdownCalls += 1;
    this.shutdownState = true;
  }

  async closeSession(): Promise<void> {}
}

function options(subc = false): AftTransportFactoryOptions {
  return {
    harness: "opencode",
    binaryPath: "/fixture/aft",
    poolOptions: {},
    configOverrides: {},
    ...(subc ? { subcConnectionFile: "/fixture/subc-connection.json" } : {}),
  };
}

function gate() {
  let open!: () => void;
  const promise = new Promise<void>((resolve) => {
    open = resolve;
  });
  return { promise, open };
}

function executionDeadline() {
  let expire: (() => void) | undefined;
  return {
    clock: {
      setTimeout(callback: () => void) {
        expire = callback;
        return 0;
      },
      clearTimeout() {},
    },
    expire() {
      expect(expire).toBeDefined();
      expire!();
    },
  };
}

describe("Location execution holds", () => {
  for (const subc of [false, true]) {
    const mode = subc ? "subc" : "standalone";

    test(`${mode}: draining holds survive same-root replacement and duplicate finalizers`, async () => {
      const created: FakePool[] = [];
      const createPool = async () => {
        const pool = new FakePool();
        created.push(pool);
        return pool;
      };
      const location = await acquireBridge("/fixture/held", options(subc), { createPool });
      const resume = gate();
      const result = withLocationExecution(location, async () => {
        await resume.promise;
        return location.toolCall("/fixture/held", {}, "write", {});
      });
      let disposed = false;
      const disposal = Promise.all([releaseBridge(location), releaseBridge(location)]).then(() => {
        disposed = true;
      });
      const replacement = await acquireBridge("/fixture/held", options(subc), { createPool });
      try {
        expect(disposed).toBe(false);
        expect(created[0]!.shutdownCalls).toBe(0);
        const refused = await withLocationExecution(location, async () => {
          throw new Error("retired Location admitted a new call");
        }).catch((error: unknown) => error);
        expect(refused).toMatchObject({ kind: "not_sent", code: "aft_module_unavailable" });
        expect((refused as Error).message).toBe(LOCATION_RELOADED_NOT_SENT);
        resume.open();
        expect(await result).toMatchObject({ success: true });
        await disposal;
        expect(created[0]!.toolCallCalls).toBe(1);
        expect(created[0]!.shutdownCalls).toBe(subc ? 1 : 0);
        expect(await replacement.toolCall("/fixture/held", {}, "read", {})).toMatchObject({
          success: true,
        });
      } finally {
        resume.open();
        await disposal;
        await releaseBridge(replacement);
      }
      expect(created.every((pool) => pool.shutdownCalls === 1)).toBe(true);
    });

    test(`${mode}: drain deadline interrupts an acknowledged mutation as unknown without replay`, async () => {
      const pool = new FakePool();
      const deadline = executionDeadline();
      const acknowledged = gate();
      const reply = gate();
      let dispatches = 0;
      pool.bridge.toolCall = async () => {
        dispatches++;
        acknowledged.open();
        await reply.promise;
        return { success: true, text: "ok" };
      };
      const location = await acquireBridge("/fixture/acknowledged", options(subc), {
        createPool: async () => pool,
        executionClock: deadline.clock,
      });
      const outcome = withLocationExecution(location, () =>
        location.getBridge("/fixture/acknowledged").toolCall("session", "write", {}),
      ).catch((error: unknown) => error);
      await acknowledged.promise;
      const disposal = releaseBridge(location);
      const replacement = await acquireBridge("/fixture/acknowledged", options(subc), {
        createPool: async () => new FakePool(),
      });
      try {
        deadline.expire();
        const error = await outcome;
        expect(error).toBeInstanceOf(BridgeTransportUnknownOutcomeError);
        expect(error).not.toMatchObject({ kind: "not_sent" });
        expect((error as Error).message).toContain("outcome is UNKNOWN");
        await disposal;
        reply.open();
        expect(dispatches).toBe(1);
        expect(pool.shutdownCalls).toBe(subc ? 1 : 0);
        expect(await replacement.toolCall("/fixture/acknowledged", {}, "read", {})).toMatchObject({
          success: true,
        });
      } finally {
        reply.open();
        await disposal;
        await releaseBridge(replacement);
      }
    });
  }

  test("drain deadline before dispatch gives a typed refusal and blocks late escaped transports", async () => {
    const pool = new FakePool();
    let dispatches = 0;
    pool.bridge.toolCall = async () => {
      dispatches++;
      return { success: true, text: "ok" };
    };
    const deadline = executionDeadline();
    const location = await acquireBridge("/fixture/no-dispatch", options(true), {
      createPool: async () => pool,
      executionClock: deadline.clock,
    });
    const resume = gate();
    const settled = gate();
    let lateError: unknown;
    const outcome = withLocationExecution(location, async () => {
      const bridge = location.getBridge("/fixture/no-dispatch");
      await resume.promise;
      try {
        return await bridge.toolCall("session", "write", {});
      } catch (error) {
        lateError = error;
        throw error;
      } finally {
        settled.open();
      }
    }).catch((error: unknown) => error);
    const disposal = releaseBridge(location);
    deadline.expire();
    const error = await outcome;
    expect(error).toMatchObject({ kind: "not_sent", code: "aft_module_unavailable" });
    expect((error as Error).message).toBe(LOCATION_RELOADED_NOT_SENT);
    await disposal;
    resume.open();
    await settled.promise;
    expect(lateError).toBe(error);
    expect(dispatches).toBe(0);
    expect(pool.toolCallCalls).toBe(0);
  });
});

describe("Location bridge lifecycle", () => {
  test("shutdown classifies an in-flight Location call as outcome unknown", async () => {
    const projectRoot = mkdtempSync(join(tmpdir(), "aft-location-lease-shutdown-"));
    const script = cachedExecutable(`#!/usr/bin/env node
process.stdin.setEncoding("utf8");
let buffer = "";
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let newline;
  while ((newline = buffer.indexOf("\\n")) !== -1) {
    const line = buffer.slice(0, newline);
    buffer = buffer.slice(newline + 1);
    const req = JSON.parse(line);
    if (req.command === "configure") {
      process.stdout.write(JSON.stringify({ id: req.id, success: true, warnings: [] }) + "\\n");
    } else {
      process.stdout.write(JSON.stringify({
        type: "progress",
        request_id: req.id,
        kind: "stdout",
        chunk: "request received",
      }) + "\\n");
    }
  }
});
`);
    const pool = new BridgePool(script, {
      idleTimeoutMs: Infinity,
      timeoutMs: 10_000,
      maxRestarts: 0,
    });
    const location = await acquireBridge(projectRoot, options(true), {
      createPool: async () => pool,
    });
    let markStarted: (() => void) | undefined;
    const started = new Promise<void>((resolve, reject) => {
      const timer = setTimeout(
        () => reject(new Error("stub transport did not receive the call")),
        5_000,
      );
      markStarted = () => {
        clearTimeout(timer);
        resolve();
      };
    });
    const outcome = location
      .toolCall(
        projectRoot,
        { sessionID: "lease-shutdown" },
        "read",
        {},
        {
          onProgress: () => markStarted?.(),
        },
      )
      .then(
        () => null,
        (error: unknown) => error,
      );

    try {
      await started;
      await location.shutdown();
      expect(await outcome).toBeInstanceOf(BridgeTransportUnknownOutcomeError);
    } finally {
      await releaseBridge(location);
      rmSync(projectRoot, { recursive: true, force: true });
    }
  });

  test("a released lease rejects before dispatch as transport unavailable", async () => {
    const pool = new FakePool();
    const location = await acquireBridge("/fixture/released-lease", options(true), {
      createPool: async () => pool,
    });

    await releaseBridge(location);
    const outcome = await Promise.resolve()
      .then(() => location.toolCall("/fixture/released-lease", {}, "read", {}))
      .then(
        () => null,
        (error: unknown) => error,
      );

    expect(outcome).toBeInstanceOf(BridgeTransportUnavailableError);
    expect(outcome).not.toBeInstanceOf(BridgeTransportUnknownOutcomeError);
    expect(pool.toolCallCalls).toBe(0);
  });

  test("two standalone Locations share one owner until the last release", async () => {
    const created: FakePool[] = [];
    const createPool = async () => {
      const pool = new FakePool();
      created.push(pool);
      return pool;
    };
    const first = await acquireBridge("/fixture/a", options(), { createPool });
    const second = await acquireBridge("/fixture/b", options(), { createPool });

    expect(created).toHaveLength(1);
    expect(getBridgeLifecycleTopology()).toEqual({
      daemonProcesses: 1,
      routes: 2,
      subcClients: 0,
      locations: 2,
    });
    created[0]!.healthResponse = {
      runtime: { live_watchers: 3, listen_ports: 0, open_routes: 2 },
      lsp: { children_total: 4 },
    };
    expect(await sampleBridgeLifecycleCensus({ settleMs: 0 })).toEqual({
      watchers: 3,
      listenPorts: 0,
      routes: 2,
      lspChildren: 4,
      daemonProcesses: 1,
    });

    await releaseBridge(first);
    expect(created[0]?.shutdownCalls).toBe(0);
    expect(getBridgeLifecycleTopology().routes).toBe(1);

    await releaseBridge(second);
    expect(created[0]?.shutdownCalls).toBe(1);
    expect(getBridgeLifecycleTopology()).toEqual({
      daemonProcesses: 0,
      routes: 0,
      subcClients: 0,
      locations: 0,
    });
    expect(await sampleBridgeLifecycleCensus({ settleMs: 0 })).toEqual({
      watchers: 0,
      listenPorts: 0,
      routes: 0,
      lspChildren: 0,
      daemonProcesses: 0,
    });
  });

  test("duplicate release is idempotent and cannot retire a sibling Location", async () => {
    const pool = new FakePool();
    const createPool = async () => pool;
    const first = await acquireBridge("/fixture/a", options(), { createPool });
    const second = await acquireBridge("/fixture/b", options(), { createPool });

    await Promise.all([releaseBridge(first), releaseBridge(first)]);
    expect(pool.shutdownCalls).toBe(0);
    expect(getBridgeLifecycleTopology().locations).toBe(1);

    await releaseBridge(second);
    expect(pool.shutdownCalls).toBe(1);
  });

  test("canonical aliases share the same serialized acquisition lock", async () => {
    const root = mkdtempSync(join(tmpdir(), "aft-location-lifecycle-"));
    const target = join(root, "project");
    const alias = join(root, "project-alias");
    const { mkdirSync } = await import("node:fs");
    mkdirSync(target);
    symlinkSync(target, alias, "dir");
    let activeCreations = 0;
    let maximumActiveCreations = 0;
    const pools: FakePool[] = [];
    const createPool = async () => {
      activeCreations += 1;
      maximumActiveCreations = Math.max(maximumActiveCreations, activeCreations);
      await Bun.sleep(10);
      activeCreations -= 1;
      const pool = new FakePool();
      pools.push(pool);
      return pool;
    };

    try {
      const [first, second] = await Promise.all([
        acquireBridge(target, options(true), { createPool }),
        acquireBridge(alias, options(true), { createPool }),
      ]);
      expect(realpathSync(alias)).toBe(realpathSync(target));
      expect(maximumActiveCreations).toBe(1);
      expect(pools).toHaveLength(2);
      expect(getBridgeLifecycleTopology()).toMatchObject({
        daemonProcesses: 0,
        routes: 2,
        subcClients: 2,
      });

      await Promise.all([releaseBridge(first), releaseBridge(second)]);
      expect(pools.map((pool) => pool.shutdownCalls)).toEqual([1, 1]);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("one of two leases for the same root cannot discard the survivor's config", async () => {
    let factoryOptions: AftTransportFactoryOptions | undefined;
    const pool = new FakePool();
    const createPool = async (received: AftTransportFactoryOptions) => {
      factoryOptions = received;
      return pool;
    };
    const first = await acquireBridge(
      "/fixture/shared",
      { ...options(), configOverrides: { location: "shared" } },
      { createPool },
    );
    const second = await acquireBridge(
      "/fixture/shared",
      { ...options(), configOverrides: { location: "shared" } },
      { createPool },
    );

    await releaseBridge(first);
    expect(factoryOptions?.poolOptions.projectConfigLoader?.("/fixture/shared")).toEqual({
      location: "shared",
    });
    expect(getBridgeLifecycleTopology().routes).toBe(1);

    await releaseBridge(second);
    expect(pool.shutdownCalls).toBe(1);
  });

  test("standalone configure state is selected per canonical Location root", async () => {
    let factoryOptions: AftTransportFactoryOptions | undefined;
    const pool = new FakePool();
    const createPool = async (received: AftTransportFactoryOptions) => {
      factoryOptions = received;
      return pool;
    };
    const first = await acquireBridge(
      "/fixture/a",
      { ...options(), configOverrides: { location: "a" } },
      { createPool },
    );
    const second = await acquireBridge(
      "/fixture/b",
      { ...options(), configOverrides: { location: "b" } },
      { createPool },
    );

    expect(factoryOptions?.poolOptions.projectConfigLoader?.("/fixture/a")).toEqual({
      location: "a",
    });
    expect(factoryOptions?.poolOptions.projectConfigLoader?.("/fixture/b")).toEqual({
      location: "b",
    });

    await Promise.all([releaseBridge(first), releaseBridge(second)]);
  });

  test("a version mismatch after the creating Location is released goes to a live Location's handler", async () => {
    let factoryOptions: AftTransportFactoryOptions | undefined;
    const pool = new FakePool();
    const createPool = async (received: AftTransportFactoryOptions) => {
      factoryOptions = received;
      return pool;
    };
    const calls: string[] = [];
    const handler = (label: string) => async (binaryVersion: string, minVersion: string) => {
      calls.push(`${label}:${binaryVersion}<${minVersion}`);
      return `/fixture/${label}/aft`;
    };
    const first = await acquireBridge(
      "/fixture/a",
      { ...options(), poolOptions: { onVersionMismatch: handler("a") } },
      { createPool },
    );
    const second = await acquireBridge(
      "/fixture/b",
      { ...options(), poolOptions: { onVersionMismatch: handler("b") } },
      { createPool },
    );

    // The pool was built from the first Location's options. In the plugin that
    // handler swaps the binary through the first Location's own lease, which
    // throws once released, so after release a live Location must answer.
    await releaseBridge(first);
    expect(await factoryOptions?.poolOptions.onVersionMismatch?.("0.1.0", "0.2.0")).toBe(
      "/fixture/b/aft",
    );
    expect(calls).toEqual(["b:0.1.0<0.2.0"]);

    await releaseBridge(second);
    expect(await factoryOptions?.poolOptions.onVersionMismatch?.("0.1.0", "0.2.0")).toBeNull();
    expect(calls).toHaveLength(1);
  });
});
