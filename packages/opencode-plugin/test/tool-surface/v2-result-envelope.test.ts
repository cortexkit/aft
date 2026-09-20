import { describe, expect, test } from "bun:test";
import type { ToolDefinition } from "@opencode-ai/plugin";
import { Effect, Exit } from "effect";

import { projectV2Tool } from "../../src/tools/definitions/v2.js";

const LOCATION = {
  directory: "/work/project",
  project: { directory: "/work/project", canonical: "/work/project" },
};
const EXECUTION_CONTEXT = {
  sessionID: "session-envelope",
  messageID: "message-envelope",
  id: "call-envelope",
  agent: "agent-envelope",
  progress: () => Effect.succeed(undefined),
};

function toolReturning(output: unknown): ReturnType<typeof projectV2Tool> {
  const definition = {
    description: "result envelope probe",
    args: {},
    execute: async () => output,
  } as unknown as ToolDefinition;
  return projectV2Tool("aft_probe", definition, LOCATION);
}

async function runExit(output: unknown) {
  return await Effect.runPromiseExit(toolReturning(output).execute({}, EXECUTION_CONTEXT));
}

describe("V2 tool result envelopes", () => {
  test("a returned failure envelope fails the call instead of reporting success", async () => {
    const message = 'Permission denied (external_directory) for "/etc/hosts".';
    const exit = await runExit(
      JSON.stringify({ success: false, code: "permission_denied", message, error: message }),
    );

    expect(Exit.isFailure(exit)).toBe(true);
  });

  test("the failure text is the rendered message, with no JSON and no duplicated field", async () => {
    const message = "no undo history for: operation";
    const error = await Effect.runPromise(
      Effect.flip(
        toolReturning(
          JSON.stringify({ success: false, code: "no_history", message, error: message }),
        ).execute({}, EXECUTION_CONTEXT),
      ),
    );

    expect(error).toBeInstanceOf(Error);
    expect((error as Error).message).toBe(message);
    expect((error as Error).message).not.toContain('"success"');
    expect((error as Error).message).not.toContain('"error"');
  });

  test("falls back to error, then code, when the envelope carries no message", async () => {
    const withError = await Effect.runPromise(
      Effect.flip(
        toolReturning(JSON.stringify({ success: false, error: "rendered failure" })).execute(
          {},
          EXECUTION_CONTEXT,
        ),
      ),
    );
    const withCode = await Effect.runPromise(
      Effect.flip(
        toolReturning(JSON.stringify({ success: false, code: "bridge_timeout" })).execute(
          {},
          EXECUTION_CONTEXT,
        ),
      ),
    );

    expect((withError as Error).message).toBe("rendered failure");
    expect((withCode as Error).message).toBe("bridge_timeout");
  });

  test("leaves ordinary output alone, including JSON that is not a failure envelope", async () => {
    const succeeded = JSON.stringify({ success: true, text: "done" });
    const unrelated = JSON.stringify({ results: [{ success: false }] });

    expect(
      await Effect.runPromise(toolReturning(succeeded).execute({}, EXECUTION_CONTEXT)),
    ).toEqual({ content: succeeded });
    expect(
      await Effect.runPromise(toolReturning(unrelated).execute({}, EXECUTION_CONTEXT)),
    ).toEqual({ content: unrelated });
    expect(
      await Effect.runPromise(toolReturning("plain text output").execute({}, EXECUTION_CONTEXT)),
    ).toEqual({ content: "plain text output" });
  });

  test("recognises the envelope when a tool reports it through a structured result", async () => {
    const message = "checkpoint restore failed";
    const error = await Effect.runPromise(
      Effect.flip(
        toolReturning({
          output: JSON.stringify({ success: false, message }),
          title: "aft_safety",
        }).execute({}, EXECUTION_CONTEXT),
      ),
    );

    expect((error as Error).message).toBe(message);
  });
});
