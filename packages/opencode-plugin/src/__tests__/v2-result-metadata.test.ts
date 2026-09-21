import { describe, expect, test } from "bun:test";
import type { ToolDefinition } from "@opencode-ai/plugin";
import { Effect, Schema } from "effect";

import { projectV2Tool } from "../tools/definitions/v2.js";

/**
 * The schema the host stores a tool part's metadata under.
 *
 * `Session.Message.ToolState.Completed.metadata` in @opencode/schema is
 * `Schema.Record(Schema.String, Schema.Json)`, and the running state's copy of
 * it is the same and not even optional. Using the real schema here is the
 * point: these tests fail for the reason the host fails, rather than against a
 * restatement of the rule that could drift away from it.
 */
const HOST_METADATA = Schema.Record(Schema.String, Schema.Json);

function storableByHost(metadata: unknown): boolean {
  try {
    Schema.decodeUnknownSync(HOST_METADATA)(metadata);
    return true;
  } catch {
    return false;
  }
}

function toolReturning(result: unknown): ToolDefinition {
  return {
    description: "fixture tool",
    args: {},
    execute: async () => result,
  } as unknown as ToolDefinition;
}

async function runProjected(
  result: unknown,
): Promise<{ output: Record<string, unknown>; progress: unknown[] }> {
  const progress: unknown[] = [];
  const projected = projectV2Tool("fixture_tool", toolReturning(result), {
    directory: "/tmp/fixture",
  });
  const output = await Effect.runPromise(
    projected.execute(
      {},
      {
        progress: (update) => {
          progress.push(update);
          return Effect.void;
        },
      },
    ),
  );
  return { output: output as Record<string, unknown>, progress };
}

describe("V2 tool result metadata", () => {
  // The shape bash returns for a foreground command: every field is optional
  // and the caller supplied none of them, so the keys are present and
  // undefined. This is the payload that used to make the host drop the whole
  // result and hand the model "Tool result missing" instead.
  const bashForegroundMetadata = {
    description: undefined,
    output: "bash-fixture\n",
    exit: 0,
    truncated: undefined,
  };

  test("the host's schema is what rejects a present-but-undefined key", () => {
    expect(storableByHost(bashForegroundMetadata)).toBe(false);
    expect(storableByHost({ output: "bash-fixture\n", exit: 0 })).toBe(true);
  });

  test("a result's metadata reaches the host storable", async () => {
    const { output } = await runProjected({
      output: "bash-fixture\n",
      title: "printf",
      metadata: bashForegroundMetadata,
    });
    const metadata = output.metadata as Record<string, unknown>;
    expect(storableByHost(metadata)).toBe(true);
    expect(Object.hasOwn(metadata, "description")).toBe(false);
    expect(Object.hasOwn(metadata, "truncated")).toBe(false);
    expect(metadata.output).toBe("bash-fixture\n");
    expect(metadata.exit).toBe(0);
    expect(metadata.title).toBe("printf");
    expect(output.content).toBe("bash-fixture\n");
  });

  test("an undefined key nested inside metadata is dropped too", async () => {
    const { output } = await runProjected({
      output: "edited",
      metadata: { filediff: { file: "a.ts", patch: undefined, additions: 2 } },
    });
    const metadata = output.metadata as Record<string, unknown>;
    expect(storableByHost(metadata)).toBe(true);
    expect(metadata.filediff).toEqual({ file: "a.ts", additions: 2 });
  });

  test("an undefined array entry becomes null, as writing it out as JSON would", async () => {
    const { output } = await runProjected({
      output: "listed",
      metadata: { files: ["a.ts", undefined, "b.ts"] },
    });
    const metadata = output.metadata as Record<string, unknown>;
    expect(storableByHost(metadata)).toBe(true);
    expect(metadata.files).toEqual(["a.ts", null, "b.ts"]);
  });

  test("progress updates are cleaned the same way", async () => {
    const definition = {
      description: "fixture tool",
      args: {},
      execute: async (_input: unknown, runtime: { metadata(update: unknown): void }) => {
        runtime.metadata({ metadata: { description: undefined, status: "running" } });
        return { output: "done" };
      },
    } as unknown as ToolDefinition;
    const progress: unknown[] = [];
    const projected = projectV2Tool("fixture_tool", definition, { directory: "/tmp/fixture" });
    await Effect.runPromise(
      projected.execute(
        {},
        {
          progress: (update) => {
            progress.push(update);
            return Effect.void;
          },
        },
      ),
    );
    // The metadata callback hands its update to a promise the tool does not
    // await, so give that promise a turn before reading what arrived.
    await new Promise((resolve) => setTimeout(resolve, 10));
    expect(progress).toHaveLength(1);
    expect(storableByHost(progress[0])).toBe(true);
    expect(progress[0]).toEqual({ status: "running" });
  });
});
