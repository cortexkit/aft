import { describe, expect, test } from "bun:test";
import { Effect } from "effect";
import { z } from "zod";

import { adaptV1Tool, makeServerEffect } from "../../src/entry/server-runtime.mjs";

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
    createTransportPool: async ({ binaryPath }: { binaryPath: string }) => {
      events.push(`pool:${binaryPath}`);
      return {
        shutdown: async () => {
          events.push("shutdown");
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

function hostContext(directory: string, events: string[], added: string[]) {
  let locationReads = 0;
  const context = {
    get location() {
      locationReads += 1;
      events.push(`location:${directory}`);
      return {
        directory,
        project: { directory, canonical: directory },
      };
    },
    tool: {
      transform: (register: (editor: { add(tool: { name: string }): void }) => void) =>
        Effect.sync(() => {
          events.push(`transform:${directory}`);
          register({
            add: (tool) => {
              added.push(tool.name);
              events.push(`add:${directory}:${tool.name}`);
            },
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
      const added: string[] = [];
      const effect = makeServerEffect(testDependencies(events));
      const host = hostContext(directory, events, added);

      const program = effect(host.context);
      expect(host.locationReads()).toBe(1);
      expect(events).toEqual([`location:${directory}`]);

      await Effect.runPromise(Effect.scoped(program));

      expect(host.locationReads()).toBe(1);
      expect(added).toEqual(["aft_probe"]);
      expect(events).toEqual([
        `location:${directory}`,
        `config:${directory}`,
        "storage",
        `configure:${directory}`,
        "binary:0.55.1",
        "pool:/isolated/bin/aft",
        "tools:/isolated/storage",
        `transform:${directory}`,
        `add:${directory}:aft_probe`,
        "shutdown",
      ]);
    }
  });

  test("adapts V1 tools to Effect execution with the host AbortSignal", async () => {
    const progress: Array<Record<string, unknown>> = [];
    let receivedContext: Record<string, unknown> | undefined;
    const tool = adaptV1Tool(
      "aft_probe",
      {
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
      {
        directory: "/work/a",
        project: { directory: "/work/a", canonical: "/canonical/a" },
      },
    );

    const result = await Effect.runPromise(
      tool.execute(
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

    expect(tool.input.safeParse({ value: "ready" }).success).toBe(true);
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
