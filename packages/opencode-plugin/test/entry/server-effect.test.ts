import { describe, expect, spyOn, test } from "bun:test";
import { subcConnectionFileError } from "@cortexkit/aft-bridge";
import { Cause, Effect } from "effect";
import { z } from "zod";

import { ConfigRejectedError } from "../../src/config.js";
import { makeServerEffect } from "../../src/entry/server-runtime.mjs";
import * as logger from "../../src/logger.js";

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
    resolvePoolOptions: () => ({ timeoutMs: 30_000, hangThreshold: 2 }),
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
      const effect = makeServerEffect(testDependencies(events));
      const host = hostContext(directory, events, added);

      const program = effect(host.context);
      expect(host.locationReads()).toBe(1);
      expect(events).toEqual([`location:${directory}`]);

      await Effect.runPromise(Effect.scoped(program));

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

  test("acquires the bridge with the Location's canonical project directory", async () => {
    const events: string[] = [];
    const host = hostContext("/work/alias", events, [], "/work/canonical");

    await Effect.runPromise(
      Effect.scoped(makeServerEffect(testDependencies(events))(host.context)),
    );

    expect(events).toContain("acquire:/work/canonical:/isolated/bin/aft");
    expect(events).toContain("release:/work/canonical");
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
    // Top-level `enabled` is retired; a rejected configuration puts the
    // Location in the config error state instead of leaving it without tools.
    const events: string[] = [];
    const added: Array<Record<string, unknown>> = [];
    const dependencies = {
      ...testDependencies(events),
      loadConfig: (directory: string) => {
        events.push(`config:${directory}`);
        throw new ConfigRejectedError(["removed_config_key:aft_glob:use:glob"], directory);
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
    await expectConfigErrorCall(added[0], "npx @cortexkit/aft doctor --fix");
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
});
