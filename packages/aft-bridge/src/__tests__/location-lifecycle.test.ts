import { describe, expect, test } from "bun:test";
import { mkdtempSync, realpathSync, rmSync, symlinkSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  acquireBridge,
  getBridgeLifecycleTopology,
  releaseBridge,
  sampleBridgeLifecycleCensus,
} from "../location-lifecycle.js";
import type { AftProjectTransport, AftTransportPool } from "../transport.js";
import type { AftTransportFactoryOptions } from "../transport-factory.js";

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

describe("Location bridge lifecycle", () => {
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
});
