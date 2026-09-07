import { describe, expect, test } from "bun:test";
import { Effect } from "effect";
import { z } from "zod";

import { makeServerEffect } from "../../src/entry/server-runtime.mjs";

function testDependencies(events: string[]) {
  return {
    loadConfig: (directory: string) => {
      events.push(`config:${directory}`);
      return {};
    },
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
      return { directory };
    },
    releaseBridge: async ({ directory }: { directory: string }) => {
      events.push(`release:${directory}`);
    },
    registerRpc: async (_context: unknown, location: { directory: string }) => {
      events.push(`rpc:${location.directory}`);
      return {
        dispose: async () => {
          events.push(`rpc-dispose:${location.directory}`);
        },
      };
    },
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
        `config:${directory}`,
        "storage",
        `configure:${directory}`,
        "binary:0.55.1",
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

  test("keeps a disabled Location inert", async () => {
    const events: string[] = [];
    const dependencies = {
      ...testDependencies(events),
      loadConfig: (directory: string) => {
        events.push(`config:${directory}`);
        return { enabled: false };
      },
    };
    const host = hostContext("/work/disabled", events, []);

    await Effect.runPromise(Effect.scoped(makeServerEffect(dependencies)(host.context)));

    expect(events).toEqual(["location:/work/disabled", "config:/work/disabled"]);
  });
});
