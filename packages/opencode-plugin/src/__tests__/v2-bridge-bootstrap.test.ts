/// <reference path="../bun-test.d.ts" />

/**
 * OpenCode 2 boot coverage for the shared bridge bootstrap.
 *
 * The OpenCode 2 runtime entry once built its bridge environment by hand and
 * never resolved ONNX Runtime, so every bridge it spawned lacked
 * `_ort_dylib_dir` and fell back to a bare `dlopen("libonnxruntime.dylib")`.
 * These tests boot the real V2 entry (with the real Location lifecycle and a
 * real BridgePool) and read the configure payload of the first bridge the pool
 * builds, which is exactly what `BinaryBridge.spawnProcess` turns into
 * `ORT_DYLIB_PATH` for the child. A parity test then boots the OpenCode 1
 * entry and the OpenCode 2 entry against the same config and compares the
 * first-bridge configure payloads key by key and value by value.
 */

import { afterEach, describe, expect, mock, spyOn, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import * as bridge from "@cortexkit/aft-bridge";
import { Effect } from "effect";
import { acquireEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import { makeServerEffect } from "../entry/server-runtime.mjs";

type OpenCodePlugin = typeof import("../index.js").default;
type Overrides = Record<string, unknown>;
type PoolOptionsLike = { projectConfigLoader?: (root: string) => Overrides };

let releaseEnv: (() => void) | undefined;
let tempDir: string | undefined;
let importCounter = 0;

afterEach(() => {
  releaseEnv?.();
  releaseEnv = undefined;
  if (tempDir) rmSync(tempDir, { recursive: true, force: true });
  tempDir = undefined;
  mock.restore();
});

/** Let background resolutions (ONNX Runtime, LSP installs) settle. */
function settle(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 20));
}

function v2Host(directory: string) {
  return {
    location: { directory, project: { directory, canonical: directory } },
    tool: { transform: () => Effect.void },
  };
}

/** Everything V2 does beyond the bridge environment is inert here. */
const inertV2Surface = {
  registerRpc: () => Effect.succeed({ dispose: async () => {} }),
  registerTools: () => Effect.void,
  toolConsumers: () => ({}),
};

/** Real Location lifecycle over a real BridgePool; bridges are built but never spawned. */
function realBridgePoolAcquire(pools: bridge.BridgePool[]) {
  return (directory: string, options: bridge.AftTransportFactoryOptions) =>
    bridge.acquireBridge(directory, options, {
      createPool: async (factory) => {
        const pool = new bridge.BridgePool(
          factory.binaryPath,
          factory.poolOptions,
          factory.configOverrides,
        );
        pools.push(pool);
        return pool as unknown as bridge.AftTransportPool;
      },
    });
}

function firstBridgeConfigure(pool: bridge.AftTransportPool, root: string): Overrides {
  const built = pool.getBridge(root) as unknown as { configOverrides: Overrides };
  return { ...built.configOverrides };
}

describe.serial("OpenCode 2 boot resolves ONNX Runtime through the shared bootstrap", () => {
  test("the first bridge the V2 entry builds carries _ort_dylib_dir", async () => {
    tempDir = mkdtempSync(join(tmpdir(), "aft-v2-ort-"));
    const project = join(tempDir, "project");
    mkdirSync(project, { recursive: true });
    const ortCalls: string[] = [];
    const pools: bridge.BridgePool[] = [];
    let configure: Overrides | undefined;

    const dependencies = {
      ...inertV2Surface,
      loadConfig: () => ({ indexes: { semantic: true }, disabled_tools: [] }),
      // The stubbed loadConfig never resets the real parse-failure registry, so
      // a parse failure recorded by an earlier test file in this process would
      // otherwise put this boot in the config error state.
      configLoadErrors: () => [],
      migrateConfigLocations: () => [],
      resolveVersion: () => "0.0.0-test",
      resolveBinary: async () => join(tempDir as string, "never-spawned-aft"),
      ensureStorageMigrated: async () => {},
      resolveStorageRoot: () => join(tempDir as string, "storage"),
      buildConfigureParams: (_directory: string, state: Overrides) => ({ ...state, config: [] }),
      ensureOnnxRuntime: async (storageDir: string) => {
        ortCalls.push(storageDir);
        return "/managed/onnxruntime/1.24.4";
      },
      startLspAutoInstall: () => null,
      pushLspPaths: async () => {},
      isOrtAutoDownloadSupported: () => true,
      acquireBridge: realBridgePoolAcquire(pools),
      releaseBridge: bridge.releaseBridge,
      buildToolMap: () => ({}),
    };

    await Effect.runPromise(
      Effect.scoped(
        Effect.gen(function* () {
          yield* makeServerEffect(dependencies)(v2Host(project));
          yield* Effect.promise(async () => {
            await settle();
            expect(pools).toHaveLength(1);
            configure = firstBridgeConfigure(pools[0] as never, project);
          });
        }),
      ),
    );

    expect(ortCalls).toEqual([join(tempDir, "storage")]);
    expect(configure?._ort_dylib_dir).toBe("/managed/onnxruntime/1.24.4");
    expect(configure?.harness).toBe("opencode");
    expect(configure?.storage_dir).toBe(join(tempDir, "storage"));
  });
});

/**
 * Stands in for the transport pool in both entries and reproduces how
 * BridgePool assembles the first bridge's configure payload: constructor
 * overrides, then runtime overrides, then the per-project loader.
 */
class RecordingPool {
  readonly runtime: Overrides = {};
  constructor(
    private readonly constructed: Overrides,
    private readonly options: PoolOptionsLike,
  ) {}
  setConfigureOverride(key: string, value: unknown): void {
    if (value === undefined) delete this.runtime[key];
    else this.runtime[key] = value;
  }
  firstSpawnConfigure(root: string): Overrides {
    return {
      ...this.constructed,
      ...this.runtime,
      ...(this.options.projectConfigLoader?.(root) ?? {}),
    };
  }
  getBridge(): never {
    throw new Error("no bridge spawns during the parity boot");
  }
  getActiveBridgeForRoot() {
    return null;
  }
  activeBridges() {
    return [];
  }
  async toolCall(): Promise<never> {
    throw new Error("no tool calls during the parity boot");
  }
  async reconfigure(): Promise<void> {}
  async replaceBinary(path: string): Promise<string> {
    return path;
  }
  async closeSession(): Promise<void> {}
  isShutdown() {
    return false;
  }
  async shutdown(): Promise<void> {}
}

describe.serial("OpenCode 1 and OpenCode 2 configure bridges identically", () => {
  test("both entries apply the same configure overrides to the first bridge", async () => {
    tempDir = mkdtempSync(join(tmpdir(), "aft-v1-v2-parity-"));
    const project = join(tempDir, "project");
    mkdirSync(project, { recursive: true });
    const configHome = join(tempDir, "config");
    mkdirSync(join(configHome, "cortexkit"), { recursive: true });
    writeFileSync(
      join(configHome, "cortexkit", "aft.jsonc"),
      JSON.stringify({ semantic_search: true, lsp: { auto_install: false } }),
    );
    releaseEnv = await acquireEnv({
      AFT_CACHE_DIR: undefined,
      AFT_STORAGE_DIR: undefined,
      AFT_BINARY_PATH: undefined,
      HOME: join(tempDir, "home"),
      XDG_CONFIG_HOME: configHome,
      XDG_CACHE_HOME: join(tempDir, "cache"),
      XDG_DATA_HOME: join(tempDir, "data"),
    });

    // The shared bootstrap's production dependencies stay real; only the
    // effects that would download, spawn, or migrate are stubbed underneath.
    spyOn(bridge, "findBinarySync").mockImplementation(() => join(tempDir as string, "aft"));
    spyOn(bridge, "ensureStorageMigrated").mockImplementation(async () => {});
    spyOn(bridge, "ensureOnnxRuntime").mockImplementation(async () => "/managed/onnxruntime");

    const v1Pools: RecordingPool[] = [];
    spyOn(bridge, "createAftTransportPool").mockImplementation(async (options) => {
      const pool = new RecordingPool(options.configOverrides ?? {}, options.poolOptions);
      v1Pools.push(pool);
      return pool as unknown as bridge.AftTransportPool;
    });
    const mod = await import(`../index.js?v1-v2-parity=${importCounter++}`);
    const plugin = mod.default as OpenCodePlugin;
    const hooks = (await plugin({
      directory: project,
      client: {},
    } as Parameters<OpenCodePlugin>[0])) as {
      dispose?: () => Promise<void>;
    };
    await settle();
    expect(v1Pools).toHaveLength(1);
    const v1Configure = (v1Pools[0] as RecordingPool).firstSpawnConfigure(project);
    await hooks.dispose?.();

    const v2Pools: RecordingPool[] = [];
    let v2Configure: Overrides | undefined;
    await Effect.runPromise(
      Effect.scoped(
        Effect.gen(function* () {
          yield* makeServerEffect({
            ...inertV2Surface,
            acquireBridge: (directory: string, options: bridge.AftTransportFactoryOptions) =>
              bridge.acquireBridge(directory, options, {
                createPool: async (factory) => {
                  const pool = new RecordingPool(
                    factory.configOverrides ?? {},
                    factory.poolOptions,
                  );
                  v2Pools.push(pool);
                  return pool as unknown as bridge.AftTransportPool;
                },
              }),
          })(v2Host(project));
          yield* Effect.promise(async () => {
            await settle();
            expect(v2Pools).toHaveLength(1);
            v2Configure = (v2Pools[0] as RecordingPool).firstSpawnConfigure(project);
          });
        }),
      ),
    );

    expect(Object.keys(v2Configure ?? {}).sort()).toEqual(Object.keys(v1Configure).sort());
    expect(v2Configure).toEqual(v1Configure);
    // The keys this parity exists for; a regression in either entry that drops
    // one fails the equality above, these name what went missing.
    for (const key of [
      "_ort_dylib_dir",
      "harness",
      "storage_dir",
      "bash_permissions",
      "lsp_auto_install_binaries",
      "aft_search_registered",
      "edit_slot_survives",
      "config",
    ]) {
      expect(v1Configure).toHaveProperty(key);
    }
  });
});
