import { Effect } from "effect";

import { makeServerEffect } from "../../entry/server-runtime.mjs";

export async function bootV2Runtime(config: Record<string, unknown> = {}) {
  const sessionHooks = new Map<string, (event: any) => Effect.Effect<unknown>>();
  const toolHooks = new Map<string, (event: any) => Effect.Effect<unknown>>();
  const tools = new Map<string, any>();
  const overrides = new Map<string, unknown>();
  const calls: Array<{ name: string; args: Record<string, unknown> }> = [];
  const bridge = {
    toolCall: async (_session: unknown, name: string, args: Record<string, unknown>) => {
      calls.push({ name, args });
      return { success: true, text: "ok" };
    },
    send: async () => ({ success: true }),
  };
  const pool = {
    setConfigureOverride: (key: string, value: unknown) => overrides.set(key, value),
    getBridge: () => bridge,
    getActiveBridgeForRoot: () => bridge,
    activeBridges: () => [bridge],
  };
  const context = {
    permission: { assert: () => Effect.void },
    agent: { get: () => Effect.succeed({ data: { permissions: [] } }) },
    location: { directory: "/work/project" },
    session: {
      get: () => Effect.succeed({}),
      prompt: () => Effect.void,
      synthetic: () => Effect.void,
      hook: (name: string, callback: (event: any) => Effect.Effect<unknown>) =>
        Effect.sync(() => {
          const previous = sessionHooks.get(name);
          sessionHooks.set(name, (event) =>
            Effect.gen(function* () {
              if (previous) yield* previous(event);
              yield* callback(event);
            }),
          );
        }),
    },
    tool: {
      hook: (name: string, callback: (event: any) => Effect.Effect<unknown>) =>
        Effect.sync(() => toolHooks.set(name, callback)),
      transform: (register: (editor: any) => void) =>
        Effect.sync(() =>
          register({ add: (tool: any) => tools.set(tool.name, tool), remove: () => {} }),
        ),
    },
    provider: {
      list: () =>
        Effect.succeed({
          data: [
            {
              id: "p",
              models: [
                { id: "vision", capabilities: { input: ["text", "image"] } },
                { id: "text", capabilities: { input: ["text"] } },
              ],
            },
          ],
        }),
    },
  };
  await Effect.runPromise(
    Effect.scoped(
      makeServerEffect({
        loadConfig: () => ({
          disabled_tools: [],
          indexes: { lexical: true, semantic: false, callgraph: true },
          ...config,
        }),
        configLoadErrors: () => [],
        configLoadSources: () => [],
        configLoadTexts: () => new Map(),
        deliverLoadNotices: () => {},
        migrateConfigLocations: () => [],
        ensureOnnxRuntime: async () => null,
        startLspAutoInstall: () => null,
        pushLspPaths: async () => {},
        resolveStorageRoot: () => "/isolated/storage",
        buildConfigureParams: () => ({}),
        resolveVersion: () => "0.58.2",
        resolveBinary: async () => "/isolated/aft",
        acquireBridge: async () => pool,
        releaseBridge: async () => {},
        registerRpc: () => Effect.succeed({ dispose: async () => {} }),
        startLiveConfigReload: () => ({ stop: () => {} }),
      })(context),
    ),
  );
  return { context, sessionHooks, toolHooks, tools, overrides, calls };
}
