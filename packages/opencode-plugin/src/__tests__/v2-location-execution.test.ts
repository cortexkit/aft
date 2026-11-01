import { expect, test } from "bun:test";
import {
  type AcquireBridgeDependencies,
  type AftProjectTransport,
  type AftTransportPool,
  acquireBridge,
  BridgeTransportUnknownOutcomeError,
  releaseBridge,
} from "@cortexkit/aft-bridge";
import { Effect, Exit, Scope, Stream } from "effect";
import { makeServerEffect } from "../entry/server-runtime.mjs";
import { _resetSubagentCacheForTest } from "../shared/subagent-detect.js";
import { callBridge, callToolCall } from "../tools/_shared.js";
import type { V2ProviderTool } from "../tools/definitions/v2.js";
import { hoistedTools } from "../tools/hoisted.js";
import type { PluginContext } from "../types.js";
import { trackV2BackgroundTask, v2StopEpoch } from "../v2-session-lifecycle.js";

function gate() {
  let open!: () => void;
  const promise = new Promise<void>((resolve) => {
    open = resolve;
  });
  return { promise, open };
}

async function rig(
  boundary: "session" | "permission" | "edit" | "bash",
  dependencies: AcquireBridgeDependencies = {},
  rpcCleanup?: { entered: ReturnType<typeof gate>; resume: ReturnType<typeof gate> },
) {
  _resetSubagentCacheForTest();
  const directory = process.cwd();
  const entered = gate();
  const resume = gate();
  const retiring = gate();
  const continued = gate();
  const calls: string[] = [];
  let shutdowns = 0;
  let disposedConsumers = false;
  const transport: AftProjectTransport = {
    send: async (command) => {
      calls.push(command);
      return { success: true, task_id: "bash-held", status: "running" };
    },
    toolCall: async (_session, name, _args, options) => {
      calls.push(options?.preview ? `${name}:preview` : name);
      return { success: true, text: "written", preview_diff: "fixture diff" };
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
    ...dependencies,
  });
  const tools = new Map<string, V2ProviderTool>();
  const pause = async () => {
    entered.open();
    await resume.promise;
  };
  const scope = await Effect.runPromise(Scope.make());
  let toolContext!: PluginContext;
  let lookups = 0;
  const host = {
    location: { directory },
    event: { subscribe: () => Stream.never },
    permission: { assert: () => Effect.void },
    session: {
      get: () =>
        Effect.promise(async () => {
          lookups++;
          if (boundary === "session") await pause();
          return {};
        }),
    },
    tool: {
      transform: (register: (editor: any) => void) =>
        Effect.sync(() => {
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
        dispose: () => {
          disposedConsumers = true;
        },
        executeBash: async ({ context }: any) => {
          // An older spawn epoch forces the real tracking helper's kill RPC.
          const epoch = v2StopEpoch(host, context.sessionID) - 1;
          await callBridge(toolContext, context, "bash", { command: "fixture" });
          entered.open();
          await resume.promise;
          try {
            await trackV2BackgroundTask(toolContext, context, "bash-held", epoch);
          } finally {
            continued.open();
          }
          return "started";
        },
      }),
      buildToolMap: (ctx: PluginContext) => {
        toolContext = ctx;
        return {
          edit: hoistedTools(ctx).edit,
          bash: {
            description: "Fixture composite bash",
            args: {},
            execute: async () => {
              throw new Error("dedicated bash executor was bypassed");
            },
          },
          write: {
            description: "Fixture mutation",
            args: {},
            execute: async (_input: unknown, runtime: any) => {
              if (boundary === "permission") {
                await runtime.ask({
                  permission: "write",
                  patterns: ["*"],
                  always: [],
                  metadata: {},
                });
              }
              return (await callToolCall(ctx, runtime, "write", { path: "test", content: "one" }))
                .text;
            },
          },
        };
      },
      registerRpc: () =>
        Effect.succeed({
          dispose: async () => {
            rpcCleanup?.entered.open();
            await rpcCleanup?.resume.promise;
          },
        }),
      registerPromptHook: () => Effect.void,
      registerToolHooks: () => Effect.void,
      registerWorkflowHints: () => Effect.void,
      startLiveConfigReload: () => ({ stop: () => {} }),
    })(host).pipe(Effect.provideService(Scope.Scope, scope)),
  );
  const toolName = boundary === "bash" ? "shell" : boundary === "edit" ? "edit" : "write";
  const invoke = () =>
    Effect.runPromise(
      tools
        .get(toolName)!
        .execute(boundary === "edit" ? { path: "test", appendContent: "one" } : {}, {
          sessionID: `reload-${boundary}`,
          progress: () => Effect.void,
        }),
    );
  const dispose = () => Effect.runPromise(Scope.close(scope, Exit.succeed(undefined)));
  return {
    calls,
    entered,
    resume,
    retiring,
    continued,
    invoke,
    dispose,
    location,
    disposedConsumers: () => disposedConsumers,
    lookups: () => lookups,
    shutdowns: () => shutdowns,
    replace: () => acquireBridge(directory, options, { createPool: async () => owner }),
  };
}

for (const boundary of ["session", "permission", "edit", "bash"] as const) {
  test(`V2 admitted tool survives reload at ${boundary} boundary without replay`, async () => {
    const r = await rig(boundary);
    let replacement: AftTransportPool | undefined;
    const pending = r.invoke().then(
      (value) => ({ value }),
      (error: unknown) => ({ error }),
    );
    await r.entered.promise;
    const disposal = r.dispose();
    try {
      await r.retiring.promise;
      replacement = await r.replace();
      expect(r.disposedConsumers()).toBe(false);
      r.resume.open();
      expect(await pending).toEqual({
        value: { content: boundary === "bash" ? "started" : "written" },
      });
      await disposal;
      expect(r.calls).toEqual(
        boundary === "bash"
          ? ["bash", "bash_kill"]
          : boundary === "edit"
            ? ["edit:preview", "edit"]
            : ["write"],
      );
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
  const error = await r.invoke().then(
    () => null,
    (error: unknown) => error,
  );
  expect(error).toMatchObject({ kind: "not_sent", code: "aft_module_unavailable" });
  expect((error as Error).message).toBe(
    "This call was not sent because the AFT plugin reloaded; retry it.",
  );
  expect(r.calls).toEqual([]);
  expect(r.lookups()).toBe(0);
});

test("V2 disposal closes admission before asynchronous registration cleanup", async () => {
  const cleanup = { entered: gate(), resume: gate() };
  const r = await rig("permission", {}, cleanup);
  r.resume.open();
  const disposal = r.dispose();
  try {
    await cleanup.entered.promise;
    const error = await r.invoke().catch((error: unknown) => error);
    expect(error).toMatchObject({ kind: "not_sent", code: "aft_module_unavailable" });
    expect(r.calls).toEqual([]);
    expect(r.lookups()).toBe(0);
  } finally {
    cleanup.resume.open();
    await disposal;
  }
});

test("V2 composite bash interrupted after spawning reports unknown, never not-started", async () => {
  let expire!: () => void;
  const r = await rig("bash", {
    executionClock: {
      setTimeout(callback) {
        expire = callback;
        return 0;
      },
      clearTimeout() {},
    },
  });
  const outcome = r.invoke().catch((error: unknown) => error);
  await r.entered.promise;
  const disposal = r.dispose();
  await r.retiring.promise;
  expect(r.calls).toEqual(["bash"]);
  expire();
  const error = await outcome;
  expect(error).toBeInstanceOf(BridgeTransportUnknownOutcomeError);
  expect(error).not.toMatchObject({ kind: "not_sent" });
  expect((error as Error).message).toContain("outcome is UNKNOWN");
  expect((error as Error).message).toContain("bash-held");
  await disposal;
  r.resume.open();
  await r.continued.promise;
  expect(r.calls).toEqual(["bash"]);
  expect(r.shutdowns()).toBe(1);
});
