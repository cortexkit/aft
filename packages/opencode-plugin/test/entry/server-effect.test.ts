import { describe, expect, spyOn, test } from "bun:test";
import { subcConnectionFileError } from "@cortexkit/aft-bridge";
import { Cause, Effect } from "effect";
import { z } from "zod";

import { ConfigRejectedError } from "../../src/config.js";
import { makeServerEffect } from "../../src/entry/server-runtime.mjs";
import * as logger from "../../src/logger.js";
import type { PluginContext } from "../../src/types.js";

/** A registered V2 tool call fails (an Effect failure, red in the host) with the fix text. */
async function expectConfigErrorCall(tool: Record<string, unknown> | undefined, fix: string) {
  const registered = tool as {
    execute(input: unknown, context: unknown): Effect.Effect<Record<string, unknown>, Error>;
  };
  const exit = await Effect.runPromiseExit(
    registered.execute({ value: "x" }, { progress: () => Effect.void }),
  );
  expect(exit._tag).toBe("Failure");
  if (exit._tag !== "Failure") return;
  const text = Cause.pretty(exit.cause);
  expect(text).toContain(fix);
  expect(text).toContain("restart");
}

function testDependencies(events: string[]) {
  return {
    loadConfig: (directory: string) => {
      events.push(`config:${directory}`);
      return {};
    },
    // The real config loader retains errors, sources and notices in module globals.
    // Return empty metadata for this fake load rather than reuse an earlier test's load.
    configLoadErrors: () => [],
    configLoadSources: () => [],
    configLoadTexts: () => new Map(),
    deliverLoadNotices: () => {},
    migrateConfigLocations: () => [],
    ensureStorageMigrated: async () => {},
    ensureOnnxRuntime: async () => null,
    startLspAutoInstall: () => null,
    pushLspPaths: async () => {},
    isOrtAutoDownloadSupported: () => true,
    resolveStorageRoot: () => {
      events.push("storage");
      return "/isolated/storage";
    },
    buildConfigureParams: (directory: string, processState: Record<string, unknown>) => {
      events.push(`configure:${directory}`);
      return { ...processState, config: [] };
    },
    resolveVersion: () => "0.55.1",
    resolveBinary: async (version: string) => {
      events.push(`binary:${version}`);
      return "/isolated/bin/aft";
    },
    resolvePoolOptions: () => ({ timeoutMs: 30_000 }),
    acquireBridge: async (directory: string, { binaryPath }: { binaryPath: string }) => {
      events.push(`acquire:${directory}:${binaryPath}`);
      return { directory, setConfigureOverride: () => {} };
    },
    releaseBridge: async ({ directory }: { directory: string }) => {
      events.push(`release:${directory}`);
    },
    registerRpc: (_context: unknown, location: { directory: string }) =>
      Effect.sync(() => {
        events.push(`rpc:${location.directory}`);
        return {
          dispose: async () => {
            events.push(`rpc-dispose:${location.directory}`);
          },
        };
      }),
    registerConfigErrorRpc: () =>
      Effect.sync(() => {
        events.push("config-error-rpc");
        return {
          dispose: async () => {
            events.push("config-error-rpc-dispose");
          },
        };
      }),
    buildToolMap: (context: { storageDir: string }, _config: unknown) => {
      events.push(`tools:${context.storageDir}`);
      return {
        aft_probe: {
          description: "Probe the V2 registration path",
          args: { value: z.string() },
          execute: async ({ value }: { value: string }) => value,
        },
      };
    },
  };
}

function hostContext(
  directory: string,
  events: string[],
  added: Array<Record<string, unknown>>,
  canonicalDirectory = directory,
) {
  let locationReads = 0;
  const context = {
    permission: { assert: () => Effect.void },
    agent: { get: () => Effect.succeed({ data: { permissions: [] } }) },
    session: { get: () => Effect.succeed(undefined) },
    get location() {
      locationReads += 1;
      events.push(`location:${directory}`);
      return {
        directory,
        project: { directory, canonical: canonicalDirectory },
      };
    },
    tool: {
      transform: (
        register: (editor: {
          add(tool: Record<string, unknown> & { name: string }): void;
          remove(name: string): void;
        }) => void,
      ) =>
        Effect.sync(() => {
          events.push(`transform:${directory}`);
          register({
            add: (tool) => {
              added.push(tool);
              events.push(`add:${directory}:${tool.name}`);
            },
            remove: (name) => events.push(`remove:${directory}:${name}`),
          });
        }),
    },
  };
  return { context, locationReads: () => locationReads };
}

describe("V2 server effect", () => {
  test("each start line names the entry and the Location, and pairs with a stop line", async () => {
    const log = spyOn(logger, "log");
    // Another suite may have replaced log with a mock that already has calls.
    // Count only runtime messages from the two Locations created in this test.
    log.mockClear();
    try {
      const events: string[] = [];
      const effect = makeServerEffect(testDependencies(events));
      // Two Locations for one directory alive at the same time: the second
      // start says so, which is what tells a double start from a reload.
      await Effect.runPromise(
        Effect.scoped(
          Effect.all([
            effect(hostContext("/work/one", events, []).context),
            effect(hostContext("/work/one", events, []).context),
          ]),
        ),
      );
      const lines = log.mock.calls
        .map((call) => String(call[0]))
        .filter((line) => line.startsWith("AFT V2 runtime"));
      expect(lines).toHaveLength(4);
      const starts = lines.filter((line) => line.includes("starting"));
      expect(starts).toHaveLength(2);
      for (const line of starts) {
        expect(line).toContain("server entry, Location (no id) at /work/one");
      }
      expect(starts[0]).not.toContain("still running");
      expect(starts[1]).toContain("1 other Location(s) for this directory still running");
      expect(lines.filter((line) => line.includes("stopped"))).toEqual([
        "AFT V2 runtime stopped (server entry, Location (no id) at /work/one)",
        "AFT V2 runtime stopped (server entry, Location (no id) at /work/one)",
      ]);
    } finally {
      log.mockRestore();
    }
  });

  test("captures and boots each Location exactly once, then releases its pool", async () => {
    for (const directory of ["/work/a", "/work/b"]) {
      const events: string[] = [];
      const added: Array<Record<string, unknown>> = [];
      let toolContext: PluginContext | undefined;
      const dependencies = testDependencies(events);
      const effect = makeServerEffect({
        ...dependencies,
        buildToolMap: (context: PluginContext, config: unknown) => {
          toolContext = context;
          return dependencies.buildToolMap(context, config);
        },
      });
      const host = hostContext(directory, events, added);
      const reads: Array<{ sessionID: string; messageID: string }> = [];
      const message = { id: "message-reader", type: "assistant", content: [] };
      Object.assign(host.context.session, {
        message: {
          get: (input: { sessionID: string; messageID: string }) =>
            Effect.sync(() => {
              reads.push(input);
              return message;
            }),
        },
      });

      const program = effect(host.context);
      expect(host.locationReads()).toBe(1);
      expect(events).toEqual([`location:${directory}`]);

      await Effect.runPromise(Effect.scoped(program));

      const reader = toolContext?.readSearchMessageV2;
      expect(reader).toBeFunction();
      expect(reads).toEqual([]);
      if (!reader) throw new Error("missing V2 search reader");
      expect(
        await Effect.runPromise(
          reader({ sessionID: "session-reader", messageID: "message-reader" }),
        ),
      ).toEqual(message);
      expect(reads).toEqual([{ sessionID: "session-reader", messageID: "message-reader" }]);

      expect(host.locationReads()).toBe(1);
      expect(added.map((tool) => tool.name)).toEqual(["aft_probe"]);
      expect(added[0]?.options).toEqual({ codemode: false });
      expect(events).toEqual([
        `location:${directory}`,
        // Loaded once to honour `enabled: false`, then again after the
        // legacy config-location migration may have moved the file.
        `config:${directory}`,
        `config:${directory}`,
        "binary:0.55.1",
        "storage",
        `configure:${directory}`,
        `acquire:${directory}:/isolated/bin/aft`,
        "tools:/isolated/storage",
        `rpc:${directory}`,
        `transform:${directory}`,
        `add:${directory}:aft_probe`,
        `rpc-dispose:${directory}`,
        `release:${directory}`,
      ]);
    }
  });

  test("acquires the bridge with the Location's own directory, not the main checkout", async () => {
    const events: string[] = [];
    const host = hostContext("/work/alias", events, [], "/work/canonical");

    await Effect.runPromise(
      Effect.scoped(makeServerEffect(testDependencies(events))(host.context)),
    );

    expect(events).toContain("acquire:/work/alias:/isolated/bin/aft");
    expect(events).toContain("release:/work/alias");
  });

  test("registers shared definitions with Effect execution and the host AbortSignal", async () => {
    const events: string[] = [];
    const registered: Array<Record<string, unknown>> = [];
    const progress: Array<Record<string, unknown>> = [];
    let receivedContext: Record<string, unknown> | undefined;
    const dependencies = {
      ...testDependencies(events),
      buildToolMap: () => ({
        aft_probe: {
          description: "Probe execution",
          args: { value: z.string() },
          execute: async (input: { value: string }, context: Record<string, unknown>) => {
            receivedContext = context;
            (context.metadata as (update: Record<string, unknown>) => void)({
              title: "Probe",
              metadata: { value: input.value },
            });
            return {
              title: "Probe",
              output: `value=${input.value}`,
              metadata: { ok: true },
            };
          },
        },
      }),
    };
    const host = hostContext("/work/a", events, registered, "/canonical/a");

    await Effect.runPromise(Effect.scoped(makeServerEffect(dependencies)(host.context)));
    const registeredTool = registered[0] as {
      input: { safeParse(input: unknown): { success: boolean } };
      options: unknown;
      execute(input: unknown, context: unknown): Effect.Effect<Record<string, unknown>>;
    };
    const result = await Effect.runPromise(
      registeredTool.execute(
        { value: "ready" },
        {
          sessionID: "session-1",
          messageID: "message-1",
          agent: "agent-1",
          progress: (update: Record<string, unknown>) =>
            Effect.sync(() => {
              progress.push(update);
            }),
        },
      ),
    );
    await Promise.resolve();

    expect(registeredTool.input.safeParse({ value: "ready" }).success).toBe(true);
    expect(registeredTool.options).toEqual({ codemode: false });
    expect(receivedContext).toMatchObject({
      sessionID: "session-1",
      messageID: "message-1",
      agent: "agent-1",
      directory: "/work/a",
      worktree: "/canonical/a",
    });
    expect(receivedContext?.abort).toBeInstanceOf(AbortSignal);
    expect(progress).toEqual([{ title: "Probe", value: "ready" }]);
    expect(result).toEqual({
      content: "value=ready",
      metadata: { ok: true, title: "Probe" },
    });
  });

  test("a rejected configuration registers failing tools and acquires no bridge", async () => {
    // A rejected configuration puts the Location in the config error state
    // instead of leaving it without tools.
    const events: string[] = [];
    const added: Array<Record<string, unknown>> = [];
    const dependencies = {
      ...testDependencies(events),
      loadConfig: (directory: string) => {
        events.push(`config:${directory}`);
        throw new ConfigRejectedError(
          ["invalid_resolved_config:missing:disabled_tools"],
          directory,
        );
      },
    };
    const host = hostContext("/work/rejected", events, added);

    await Effect.runPromise(Effect.scoped(makeServerEffect(dependencies)(host.context)));

    expect(events).toEqual([
      "location:/work/rejected",
      "config:/work/rejected",
      "tools:",
      "config-error-rpc",
      "transform:/work/rejected",
      "add:/work/rejected:aft_probe",
      "config-error-rpc-dispose",
    ]);
    await expectConfigErrorCall(added[0], "invalid_resolved_config:missing:disabled_tools");
  });

  test("a config file that does not parse registers failing tools and acquires no bridge", async () => {
    const events: string[] = [];
    const added: Array<Record<string, unknown>> = [];
    const dependencies = {
      ...testDependencies(events),
      configLoadErrors: () => [{ path: "/work/p/aft.jsonc", message: "Unexpected end" }],
    };
    const host = hostContext("/work/p", events, added);

    await Effect.runPromise(Effect.scoped(makeServerEffect(dependencies)(host.context)));

    expect(events.some((event) => event.startsWith("acquire:"))).toBe(false);
    expect(events.some((event) => event.startsWith("binary:"))).toBe(false);
    expect(added.map((tool) => tool.name)).toEqual(["aft_probe"]);
    await expectConfigErrorCall(added[0], "Fix the JSONC syntax in that file");
  });

  test("a missing subc connection file registers failing tools and acquires no bridge", async () => {
    const events: string[] = [];
    const added: Array<Record<string, unknown>> = [];
    const dependencies = {
      ...testDependencies(events),
      loadConfig: () => ({ subc: { connection_file: "/nowhere/subc.json" } }),
      // The real check, against a path that does not exist.
      subcConnectionFileError,
    };
    const host = hostContext("/work/subc", events, added);

    await Effect.runPromise(Effect.scoped(makeServerEffect(dependencies)(host.context)));

    expect(events.some((event) => event.startsWith("acquire:"))).toBe(false);
    expect(events.some((event) => event.startsWith("binary:"))).toBe(false);
    expect(events).toContain("config-error-rpc");
    await expectConfigErrorCall(
      added[0],
      "Start the Subconscious daemon, correct the path, or remove subc.connection_file",
    );
  });

  test("returns a no-op when the host context has no location", async () => {
    const events: string[] = [];
    const effect = makeServerEffect(testDependencies(events));

    await Effect.runPromise(Effect.scoped(effect({})));
    await Effect.runPromise(Effect.scoped(effect({ location: undefined })));
    await Effect.runPromise(Effect.scoped(effect({ location: { directory: 1 } })));

    expect(events).toEqual([]);
  });

  // OpenCode 2 has no chat.message hook, so without this registration a new
  // message could not detach a waiting bash there.
  test("registers a session prompt hook that detaches through the Location's pool and live config", async () => {
    const events: string[] = [];
    const detached: string[] = [];
    const routed: string[] = [];
    const hooks: Array<{ name: string; callback: (event: unknown) => Effect.Effect<void> }> = [];
    let setConfig: ((next: Record<string, unknown>) => void) | undefined;
    const bridge = {
      send: async (command: string, params: Record<string, unknown>) => {
        if (command === "bash_wait_detach") detached.push(String(params.session_id));
        return { success: true, detached: true };
      },
    };
    const dependencies = {
      ...testDependencies(events),
      loadConfig: () => ({ bash: { detach_on_user_message: false } }),
      toolConsumers: () => ({ registerSession: (sessionID: string) => routed.push(sessionID) }),
      acquireBridge: async (directory: string) => ({
        directory,
        setConfigureOverride: () => {},
        getActiveBridgeForRoot: (root: string) => {
          events.push(`active-root:${root}`);
          return bridge;
        },
        activeBridges: () => [bridge],
      }),
      startLiveConfigReload: (options: { setConfig(next: Record<string, unknown>): void }) => {
        setConfig = options.setConfig;
        return { stop: () => {} };
      },
    };
    const host = hostContext("/work/a", events, [], "/canonical/a");
    const context = {
      ...host.context,
      location: host.context.location,
      session: {
        hook: (name: string, callback: (event: unknown) => Effect.Effect<void>) =>
          Effect.sync(() => {
            hooks.push({ name, callback });
            return { dispose: Effect.void };
          }),
      },
    };

    await Effect.runPromise(Effect.scoped(makeServerEffect(dependencies)(context)));

    // Editing tools are now gated in auxiliary requests as well as primary context.
    expect(hooks.map((hook) => hook.name)).toEqual([
      "prompt",
      "context",
      "compaction",
      "generate",
      "context",
    ]);
    const prompt = hooks[0]?.callback;
    if (!prompt) return;
    const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

    // The config the Location booted with keeps plain messages from detaching.
    await Effect.runPromise(prompt({ sessionID: "s1", prompt: { text: "still there?" } }));
    await settle();
    expect(detached).toEqual([]);
    expect(routed).toEqual(["s1"]);

    // A live reload that turns the setting on applies to the next message.
    setConfig?.({ bash: { detach_on_user_message: true } });
    const event = { sessionID: "s1", prompt: { text: "still there? &detach" } };
    await Effect.runPromise(prompt(event));
    await settle();
    expect(detached).toEqual(["s1"]);
    expect(event.prompt.text).toBe("still there? ");
    // The first bridge asked is the one the Location's tools run on, keyed by
    // the Location's own directory rather than the canonical main checkout the
    // pool was acquired for, so a Location in a linked worktree detaches its
    // own wait first.
    expect(events).toContain("active-root:/work/a");
    expect(events).not.toContain("active-root:/canonical/a");
  });
});

test("V2 boot rejection logs the Location once before the host ignores its cause", async () => {
  const logged = spyOn(logger, "error");
  logged.mockClear();
  try {
    const events: string[] = [];
    const dependencies = {
      ...testDependencies(events),
      resolveBinary: async () => {
        throw new Error("boot failed");
      },
    };
    await Effect.runPromise(
      Effect.scoped(
        makeServerEffect(dependencies)(hostContext("/work/broken", events, []).context),
      ).pipe(Effect.ignoreCause),
    );
    const lines = logged.mock.calls
      .map((call) => String(call[0]))
      .filter((line) => line.includes("boot failed"));
    expect(lines).toHaveLength(1);
    expect(lines[0]).toContain("/work/broken");
  } finally {
    logged.mockRestore();
  }
});
