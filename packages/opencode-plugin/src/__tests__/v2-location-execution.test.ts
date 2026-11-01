import { expect, test } from "bun:test";
import { acquireBridge, type AftTransportPool, releaseBridge } from "@cortexkit/aft-bridge";
import { Effect, Exit, Scope } from "effect";
import { makeServerEffect } from "../entry/server-runtime.mjs";
import { _resetSubagentCacheForTest } from "../shared/subagent-detect.js";
import { callToolCall } from "../tools/_shared.js";
import type { V2ProviderTool } from "../tools/definitions/v2.js";
import type { PluginContext } from "../types.js";

function gate() {
  let open!: () => void;
  const promise = new Promise<void>((resolve) => {
    open = resolve;
  });
  return { promise, open };
}

async function rig(boundary: "session" | "permission") {
  _resetSubagentCacheForTest();
  const directory = process.cwd();
  const entered = gate();
  const resume = gate();
  const retiring = gate();
  const calls: string[] = [];
  let shutdowns = 0;
  let disposedConsumers = false;
  const transport = {
    send: async () => ({ success: true }),
    toolCall: async (_session: unknown, name: string) => {
      calls.push(name);
      return { success: true, text: "written" };
    },
    getCwd: () => directory,
    getCachedStatus: () => null,
    cacheStatusSnapshot: () => {},
  };
  const owner: AftTransportPool = {
    getBridge: () => transport,
    getActiveBridgeForRoot: () => transport,
    activeBridges: () => [transport],
    toolCall: async () => ({ success: true, text: "ok" }),
    setConfigureOverride: () => {},
    reconfigure: async () => {},
    replaceBinary: async (path) => path,
    closeSession: async () => {},
    isShutdown: () => shutdowns > 0,
    shutdown: async () => {
      shutdowns++;
    },
  };
  const options = {
    harness: "opencode" as const,
    binaryPath: "/fixture/aft",
    poolOptions: {},
    configOverrides: {},
  };
  const location = await acquireBridge(directory, options, {
    createPool: async () => owner,
  });
  const tools = new Map<string, V2ProviderTool>();
  const pause = async () => {
    entered.open();
    await resume.promise;
  };
  const scope = await Effect.runPromise(Scope.make());
  const host = {
    location: { directory },
    permission: { assert: () => Effect.void },
    session: {
      get: () => Effect.promise(async () => {
        if (boundary === "session") await pause();
        return {};
      }),
    },
    tool: {
      transform: (register: (editor: any) => void) => Effect.sync(() => {
        register({ add: (tool: V2ProviderTool) => tools.set(tool.name, tool), remove: () => {} });
      }),
    },
  };
  await Effect.runPromise(
    makeServerEffect({
      loadConfig: () => ({ disabled_tools: [], indexes: { semantic: false } }),
      configLoadErrors: () => [],
      configLoadSources: () => [],
      configLoadTexts: () => new Map(),
      migrateConfigLocations: () => [],
      deliverLoadNotices: () => {},
      resolveVersion: () => "test",
      resolveBinary: async () => "/fixture/aft",
      resolveStorageRoot: () => "/fixture/storage",
      ensureStorageMigrated: async () => {},
      buildConfigureParams: () => ({}),
      startLspAutoInstall: () => null,
      isOrtAutoDownloadSupported: () => false,
      acquireBridge: async () => location,
      releaseBridge: (pool: AftTransportPool) => {
        const pending = releaseBridge(pool);
        retiring.open();
        return pending;
      },
      toolConsumers: () => ({
        requestPermission: pause,
        dispose: () => { disposedConsumers = true; },
      }),
      buildToolMap: (ctx: PluginContext) => ({
        write: {
          description: "Fixture mutation",
          args: {},
          execute: async (_input: unknown, runtime: any) => {
            if (boundary === "permission") {
              await runtime.ask({ permission: "write", patterns: ["*"], always: [], metadata: {} });
            }
            return (await callToolCall(ctx, runtime, "write", { path: "test", content: "one" })).text;
          },
        },
      }),
      registerRpc: () => Effect.succeed({ dispose: async () => {} }),
      registerPromptHook: () => Effect.void,
      registerToolHooks: () => Effect.void,
      registerWorkflowHints: () => Effect.void,
      startLiveConfigReload: () => ({ stop: () => {} }),
    })(host).pipe(Effect.provideService(Scope.Scope, scope)),
  );
  const invoke = () => Effect.runPromise(tools.get("write")!.execute({}, {
    sessionID: `reload-${boundary}`,
    progress: () => Effect.void,
  }));
  const dispose = () => Effect.runPromise(Scope.close(scope, Exit.succeed(undefined)));
  return {
    calls, entered, resume, retiring, invoke, dispose, location,
    disposedConsumers: () => disposedConsumers,
    shutdowns: () => shutdowns,
    replace: () => acquireBridge(directory, options, { createPool: async () => owner }),
  };
}

for (const boundary of ["session", "permission"] as const) {
  test(`V2 admitted write survives reload during ${boundary} lookup without replay`, async () => {
    const r = await rig(boundary);
    let replacement: AftTransportPool | undefined;
    const pending = r.invoke().then((value) => ({ value }), (error: unknown) => ({ error }));
    await r.entered.promise;
    const disposal = r.dispose();
    try {
      await r.retiring.promise;
      replacement = await r.replace();
      r.resume.open();
      expect(await pending).toEqual({ value: { content: "written" } });
      await disposal;
      expect(r.calls).toEqual(["write"]);
      expect(r.shutdowns()).toBe(0);
      expect(r.disposedConsumers()).toBe(true);
    } finally {
      r.resume.open();
      await disposal;
      if (replacement) await releaseBridge(replacement);
      await releaseBridge(r.location);
    }
  });
}

test("V2 stale tool closure refuses after disposal without lookup or dispatch", async () => {
  const r = await rig("session");
  r.resume.open();
  await r.dispose();
  const error = await r.invoke().then(() => null, (error: unknown) => error);
  expect(error).toMatchObject({ kind: "not_sent", code: "aft_module_unavailable" });
  expect((error as Error).message).toBe("This call was not sent because the AFT plugin reloaded; retry it.");
  expect(r.calls).toEqual([]);
});
