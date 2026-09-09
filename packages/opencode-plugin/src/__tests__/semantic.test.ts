/// <reference path="../bun-test.d.ts" />
import { afterAll, beforeAll, describe, expect, mock, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { semanticTools } from "../tools/semantic.js";
import type { PluginContext } from "../types.js";
import { mockAsk, mockAskDeny, noopAsk } from "./test-helpers";

let projectRoot: string;

beforeAll(() => {
  projectRoot = mkdtempSync(join(tmpdir(), "aft-test-repo-"));
});

afterAll(() => {
  rmSync(projectRoot, { recursive: true, force: true });
});

type BridgeResponse = Record<string, unknown>;
type SendCall = { command: string; params: Record<string, unknown> };
type ToolCallCall = {
  sessionId: string | undefined;
  name: string;
  rawArgs: Record<string, unknown>;
  options?: Record<string, unknown>;
};
type BridgeCall = { projectRoot: string };

function createMockClient(): any {
  return {
    lsp: {
      status: async () => ({ data: [] }),
    },
    find: {
      symbols: async () => ({ data: [] }),
    },
  };
}

function createPluginContext(pool: BridgePool, config: Record<string, unknown>): PluginContext {
  return {
    pool,
    client: createMockClient(),
    config: config as PluginContext["config"],
    storageDir: "/tmp/aft-test",
  };
}

function createMockSdkContext(directory = projectRoot, ask = noopAsk): ToolContext {
  return {
    sessionID: "semantic-session",
    messageID: "message-id",
    agent: "test",
    directory,
    worktree: directory,
    abort: new AbortController().signal,
    metadata: () => {},
    ask,
  };
}

function createMockSemanticHarness(
  config: Record<string, unknown>,
  sendImpl: (
    command: string,
    params: Record<string, unknown>,
  ) => Promise<BridgeResponse> | BridgeResponse,
) {
  const sendCalls: SendCall[] = [];
  const toolCallCalls: ToolCallCall[] = [];
  const bridgeCalls: BridgeCall[] = [];
  const bridge = {
    send: async (command: string, params: Record<string, unknown> = {}) => {
      sendCalls.push({ command, params });
      return await sendImpl(command, params);
    },
    toolCall: async (
      sessionId: string | undefined,
      name: string,
      rawArgs: Record<string, unknown> = {},
      options?: Record<string, unknown>,
    ) => {
      toolCallCalls.push({ sessionId, name, rawArgs, options });
      return await sendImpl(name, rawArgs);
    },
  };

  const pool = {
    getBridge: (projectRoot: string) => {
      bridgeCalls.push({ projectRoot });
      return bridge;
    },
  } as unknown as BridgePool;

  return {
    bridgeCalls,
    sendCalls,
    toolCallCalls,
    tools: semanticTools(createPluginContext(pool, config)),
  };
}

describe("semanticTools", () => {
  test("registers aft_search", () => {
    const { tools } = createMockSemanticHarness({}, () => ({ success: true }));

    expect(Object.keys(tools)).toEqual(["aft_search"]);
  });

  test("schema does not advertise the legacy hint property", () => {
    const { tools } = createMockSemanticHarness({}, () => ({ success: true, text: "ok" }));

    expect((tools.aft_search.args as Record<string, unknown>).hint).toBeUndefined();
  });

  test("returns ONLY the clean text (no structured JSON dump) and sends params", async () => {
    const sdkCtx = createMockSdkContext(projectRoot);
    const bridgeResponse = {
      success: true,
      text: "src/auth.ts\nvalidateToken [function] lines 10-32\n\nFound 1 result(s).",
      interpreted_as: "hybrid",
      semantic_status: "ready",
      more_available: true,
      engine_capped: false,
      fully_degraded: false,
      warnings: ["short_query_rerouted"],
      results: [
        {
          file: "src/auth.ts",
          name: "validateToken",
          kind: "function",
          source: "hybrid",
          score: 0.913,
          semantic_score: 0.9,
        },
      ],
    };
    const { bridgeCalls, sendCalls, toolCallCalls, tools } = createMockSemanticHarness(
      {},
      () => bridgeResponse,
    );

    const output = await tools.aft_search.execute(
      { query: "authentication logic", topK: 5 },
      sdkCtx,
    );

    // The mock records server-side tool calls separately from direct bridge sends.
    expect(bridgeCalls.length).toBe(1);
    expect(sendCalls).toEqual([]);
    expect(toolCallCalls).toEqual([
      {
        sessionId: "semantic-session",
        name: "search",
        rawArgs: {
          query: "authentication logic",
          topK: 5,
        },
        options: expect.objectContaining({
          timeoutMs: 60_000,
          abortSignal: sdkCtx.abort,
        }),
      },
    ]);
    // The agent gets exactly Rust's clean text — no JSON dump, no leaked
    // score/semantic_score/source/path fields.
    expect(output).toBe(bridgeResponse.text);
    expect(output).not.toContain("Structured response");
    expect(output).not.toContain("semantic_score");
    expect(output).not.toContain("0.913");
    expect(output).not.toContain('"source"');
  });

  test("passes includeTests through as a raw tool_call argument", async () => {
    const sdkCtx = createMockSdkContext(projectRoot);
    const { toolCallCalls, tools } = createMockSemanticHarness({}, () => ({
      success: true,
      text: "ok",
    }));

    await tools.aft_search.execute({ query: "fixtures", includeTests: true }, sdkCtx);

    expect(toolCallCalls[0].rawArgs.includeTests).toBe(true);
  });

  test("rejects blank queries before permission or bridge calls", async () => {
    const ask = mockAsk();
    const sdkCtx = createMockSdkContext(projectRoot, ask);
    const sendImpl = mock(() => ({ success: true, text: "should not call" }));
    const { sendCalls, toolCallCalls, tools } = createMockSemanticHarness({}, sendImpl);

    await expect(tools.aft_search.execute({ query: "   " }, sdkCtx)).rejects.toThrow(
      "invalid params",
    );

    expect(ask).not.toHaveBeenCalled();
    expect(sendCalls).toEqual([]);
    expect(toolCallCalls).toEqual([]);
    expect(sendImpl).not.toHaveBeenCalled();
  });

  test("returns server-rendered honesty text without appending plugin-side notes", async () => {
    const sdkCtx = createMockSdkContext(projectRoot);
    const { tools } = createMockSemanticHarness({}, () => ({
      success: true,
      text: "partial results\n\nFound 2 result(s). More results available; raise topK to see more.\nSearch status: fully degraded; partial/incomplete.",
      more_available: true,
      engine_capped: true,
      fully_degraded: true,
      complete: false,
      results: [],
    }));

    const output = await tools.aft_search.execute({ query: "auth", topK: 5 }, sdkCtx);

    // Rust's text is preserved verbatim...
    expect(output).toContain("partial results");
    expect(output).toContain("More results available; raise topK to see more.");
    expect(output).toContain("Search status: fully degraded; partial/incomplete.");
    expect(output).not.toContain("enumeration capped");
    expect(output).not.toContain("Structured response");
  });

  test("throws semantic runtime errors with code and message", async () => {
    const sdkCtx = createMockSdkContext(projectRoot);
    const { tools } = createMockSemanticHarness({}, () => ({
      success: false,
      code: "semantic_search_unavailable",
      message: "Semantic search unavailable: ONNX Runtime not installed.",
      text: "semantic_search: semantic_search_unavailable — Semantic search unavailable: ONNX Runtime not installed.",
    }));

    await expect(
      tools.aft_search.execute({ query: "authentication logic", topK: 5 }, sdkCtx),
    ).rejects.toThrow(
      "semantic_search: semantic_search_unavailable — Semantic search unavailable: ONNX Runtime not installed.",
    );
  });

  test("throws bridge failure envelopes with their message", async () => {
    const sdkCtx = createMockSdkContext(projectRoot);
    const { tools } = createMockSemanticHarness({}, () => ({
      success: false,
      code: "permission_required",
      message: "grep permission required",
      text: "semantic_search: permission_required — grep permission required",
    }));

    await expect(tools.aft_search.execute({ query: "TODO", topK: 5 }, sdkCtx)).rejects.toThrow(
      "semantic_search: permission_required — grep permission required",
    );
  });

  test("asks permission for every route and ignores legacy hints", async () => {
    for (const hint of ["regex", "literal", "semantic", "auto"] as const) {
      const ask = mockAsk();
      const sdkCtx = createMockSdkContext(projectRoot, ask);
      const { toolCallCalls, tools } = createMockSemanticHarness({}, () => ({
        success: true,
        text: "ok",
      }));

      await tools.aft_search.execute({ query: "TODO", hint } as never, sdkCtx);

      expect(ask).toHaveBeenCalledTimes(1);
      expect(toolCallCalls[0].rawArgs).toEqual({ query: "TODO" });
    }
  });

  test("permission denied returns an error envelope without bridge call", async () => {
    const sdkCtx = createMockSdkContext(projectRoot, mockAskDeny("Denied by policy"));
    const sendImpl = mock(() => ({ success: true, text: "should not call" }));
    const { sendCalls, toolCallCalls, tools } = createMockSemanticHarness({}, sendImpl);

    const output = await tools.aft_search.execute({ query: "TODO", hint: "literal" }, sdkCtx);

    expect(sendCalls).toEqual([]);
    expect(toolCallCalls).toEqual([]);
    expect(sendImpl).not.toHaveBeenCalled();
    expect(output).toContain("permission_denied");
    expect(output).toContain("Denied by policy");
  });
});
