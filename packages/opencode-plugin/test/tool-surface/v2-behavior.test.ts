import { describe, expect, test } from "bun:test";
import { Effect } from "effect";

import { makeServerEffect } from "../../src/entry/server-runtime.mjs";
import { SHARED_BEHAVIOR_CASES } from "./behavior-cases.js";

describe("OpenCode V2 shared behavioral cases", () => {
  for (const behavior of SHARED_BEHAVIOR_CASES) {
    test(`${behavior.name} runs through V1 and the Effect entry from one definition`, async () => {
      const v1 = await behavior.definition.execute(behavior.input, {
        directory: "/work/project",
        worktree: "/work/canonical",
        sessionID: "behavior-v1",
      } as never);
      expect(v1).toEqual(behavior.v1Output);

      const registered: Array<Record<string, unknown>> = [];
      const location = {
        directory: "/work/project",
        project: { directory: "/work/project", canonical: "/work/canonical" },
      };
      const context = {
        location,
        tool: {
          transform: (
            transform: (editor: {
              add(definition: Record<string, unknown>): void;
              remove(name: string): void;
            }) => void,
          ) =>
            Effect.sync(() =>
              transform({
                add: (definition) => registered.push(definition),
                remove: () => {},
              }),
            ),
        },
      };
      const dependencies = {
        loadConfig: () => ({}),
        resolveStorageRoot: () => "/isolated/storage",
        buildConfigureParams: () => ({}),
        resolveVersion: () => "test",
        resolveBinary: async () => "/isolated/bin/aft",
        resolvePoolOptions: () => ({}),
        acquireBridge: async () => ({}),
        releaseBridge: async () => {},
        buildToolMap: () => ({ [behavior.name]: behavior.definition }),
      };

      const v2 = await Effect.runPromise(
        Effect.scoped(
          Effect.gen(function* () {
            yield* makeServerEffect(dependencies)(context);
            const projected = registered[0] as {
              options: { codemode: boolean };
              execute(input: unknown, context: unknown): Effect.Effect<unknown>;
            };
            expect(projected.options.codemode).toBe(false);
            return yield* projected.execute(behavior.input, {
              sessionID: "behavior-v2",
              messageID: "message-v2",
              agent: "fixture",
              progress: () => Effect.succeed(undefined),
            });
          }),
        ),
      );

      expect(v2).toEqual(behavior.v2Output);
    });
  }
});
