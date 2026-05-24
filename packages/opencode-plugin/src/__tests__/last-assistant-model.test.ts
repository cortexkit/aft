/// <reference path="../bun-test.d.ts" />
import { describe, expect, test } from "bun:test";
import { getLastAssistantModel, resolvePromptContext } from "../shared/last-assistant-model.js";

function makeClient(messages: unknown[]) {
  return {
    session: {
      messages: async (_input: { path: { id: string }; query?: { limit?: number } }) => ({
        data: messages,
      }),
    },
  };
}

/**
 * Build a fake `client.session.messages` that records every input it was
 * called with — used by the "bounded request" tests below to prove we send
 * `query.limit` on every call.
 */
function makeRecordingClient(messages: unknown[]) {
  const calls: Array<{ path: { id: string }; query?: { limit?: number } }> = [];
  return {
    calls,
    client: {
      session: {
        messages: async (input: { path: { id: string }; query?: { limit?: number } }) => {
          calls.push(input);
          return { data: messages };
        },
      },
    },
  };
}

describe("resolvePromptContext (xtra-style: reads from messages API)", () => {
  test("reads flat-shape AssistantMessage info", async () => {
    const client = makeClient([
      {
        info: {
          role: "assistant",
          agent: "build",
          providerID: "anthropic",
          modelID: "claude-opus-4-7",
          variant: "thinking",
        },
      },
    ]);
    const result = await resolvePromptContext(client, "s1");
    expect(result).toEqual({
      agent: "build",
      model: { providerID: "anthropic", modelID: "claude-opus-4-7" },
      variant: "thinking",
    });
  });

  test("reads nested-shape UserMessage info as fallback when no assistant has fields", async () => {
    const client = makeClient([
      {
        info: {
          role: "user",
          agent: "build",
          model: {
            providerID: "anthropic",
            modelID: "claude-opus-4-7",
            variant: "thinking",
          },
        },
      },
    ]);
    const result = await resolvePromptContext(client, "s1");
    expect(result).toEqual({
      agent: "build",
      model: { providerID: "anthropic", modelID: "claude-opus-4-7" },
      variant: "thinking",
    });
  });

  test("prefers assistant role over user role", async () => {
    const client = makeClient([
      {
        info: {
          role: "user",
          agent: "build",
          model: { providerID: "openai", modelID: "gpt-4o", variant: "high" },
        },
      },
      {
        info: {
          role: "assistant",
          agent: "build",
          providerID: "anthropic",
          modelID: "claude-opus-4-7",
          variant: "thinking",
        },
      },
    ]);
    const result = await resolvePromptContext(client, "s1");
    expect(result?.model?.modelID).toBe("claude-opus-4-7");
    expect(result?.variant).toBe("thinking");
  });

  test("walks newest-first within same role", async () => {
    const client = makeClient([
      {
        info: {
          role: "assistant",
          agent: "build",
          providerID: "openai",
          modelID: "gpt-4o",
          variant: "high",
        },
      },
      {
        info: {
          role: "assistant",
          agent: "build",
          providerID: "anthropic",
          modelID: "claude-opus-4-7",
          variant: "thinking",
        },
      },
    ]);
    const result = await resolvePromptContext(client, "s1");
    expect(result?.model?.modelID).toBe("claude-opus-4-7");
  });

  test("merges fields across messages — agent from one, model+variant from another", async () => {
    const client = makeClient([
      {
        info: {
          role: "assistant",
          providerID: "anthropic",
          modelID: "claude-opus-4-7",
          variant: "thinking",
          // no agent
        },
      },
      {
        // older user message provides agent
        info: {
          role: "user",
          agent: "build",
        },
      },
    ]);
    const result = await resolvePromptContext(client, "s1");
    expect(result).toEqual({
      agent: "build",
      model: { providerID: "anthropic", modelID: "claude-opus-4-7" },
      variant: "thinking",
    });
  });

  test("returns null on empty messages array", async () => {
    const client = makeClient([]);
    expect(await resolvePromptContext(client, "s1")).toBeNull();
  });

  test("returns null when client.session.messages is unavailable", async () => {
    const result = await resolvePromptContext({}, "s1");
    expect(result).toBeNull();
  });

  test("returns null when the messages API throws", async () => {
    const client = {
      session: {
        messages: async () => {
          throw new Error("boom");
        },
      },
    };
    expect(await resolvePromptContext(client, "s1")).toBeNull();
  });

  test("accepts response shape without `data` wrapper (raw array)", async () => {
    const client = {
      session: {
        messages: async () => [
          {
            info: {
              role: "assistant",
              agent: "build",
              providerID: "anthropic",
              modelID: "claude-opus-4-7",
            },
          },
        ],
      },
    };
    const result = await resolvePromptContext(client, "s1");
    expect(result?.agent).toBe("build");
  });

  test("ignores model entries missing providerID or modelID", async () => {
    const client = makeClient([
      {
        info: {
          role: "assistant",
          providerID: "anthropic",
          // missing modelID
        },
      },
    ]);
    expect(await resolvePromptContext(client, "s1")).toBeNull();
  });
});

describe("getLastAssistantModel (compatibility shim)", () => {
  test("returns the resolved model + variant", async () => {
    const client = makeClient([
      {
        info: {
          role: "assistant",
          providerID: "anthropic",
          modelID: "claude-opus-4-7",
          variant: "thinking",
        },
      },
    ]);
    expect(await getLastAssistantModel(client, "s1")).toEqual({
      providerID: "anthropic",
      modelID: "claude-opus-4-7",
      variant: "thinking",
    });
  });

  test("returns null when no model can be resolved", async () => {
    const client = makeClient([{ info: { role: "assistant", agent: "build" } }]);
    expect(await getLastAssistantModel(client, "s1")).toBeNull();
  });

  test("omits variant key when none was found", async () => {
    const client = makeClient([
      {
        info: {
          role: "assistant",
          providerID: "anthropic",
          modelID: "claude-opus-4-7",
        },
      },
    ]);
    const result = await getLastAssistantModel(client, "s1");
    expect(result).toEqual({
      providerID: "anthropic",
      modelID: "claude-opus-4-7",
    });
    expect("variant" in (result as object)).toBe(false);
  });
});

// Regression coverage for the unbounded-messages-call bug surfaced by
// OpenCode's plugin agent: legacy `client.session.messages()` without a
// `query.limit` hydrates the entire session (30k-45k messages, 100k+ parts
// on large legacy sessions). These tests pin the bounded contract so
// future edits cannot accidentally drop the limit.
describe("resolvePromptContext: bounded SDK call", () => {
  test("sends query.limit on every request", async () => {
    const { calls, client } = makeRecordingClient([
      {
        info: {
          role: "assistant",
          agent: "build",
          providerID: "anthropic",
          modelID: "claude-opus-4-7",
          variant: "thinking",
        },
      },
    ]);
    await resolvePromptContext(client, "s1");
    expect(calls).toHaveLength(1);
    expect(calls[0].path).toEqual({ id: "s1" });
    expect(calls[0].query).toBeDefined();
    expect(typeof calls[0].query?.limit).toBe("number");
  });

  test("limit is a small positive integer (not unbounded)", async () => {
    const { calls, client } = makeRecordingClient([]);
    await resolvePromptContext(client, "s1");
    const limit = calls[0]?.query?.limit;
    expect(limit).toBeDefined();
    expect(limit).toBeGreaterThan(0);
    // 200 is a defensive ceiling — the actual constant is 50; if it ever
    // grows past 200 we want a deliberate review, not a silent regression.
    expect(limit).toBeLessThanOrEqual(200);
  });

  test("extraction still works correctly under the bounded call", async () => {
    const { client } = makeRecordingClient([
      {
        info: {
          role: "assistant",
          agent: "build",
          providerID: "anthropic",
          modelID: "claude-opus-4-7",
          variant: "thinking",
        },
      },
    ]);
    const result = await resolvePromptContext(client, "s1");
    expect(result).toEqual({
      agent: "build",
      model: { providerID: "anthropic", modelID: "claude-opus-4-7" },
      variant: "thinking",
    });
  });
});
