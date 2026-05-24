/// <reference path="../bun-test.d.ts" />
import { describe, expect, test } from "bun:test";
import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { semanticTools } from "../tools/semantic.js";
import type { PluginContext } from "../types.js";
import { noopAsk } from "./test-helpers";

type BridgeResponse = Record<string, unknown>;
type SendCall = { command: string; params: Record<string, unknown> };
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

function createMockSdkContext(directory = "/tmp/semantic-tests"): ToolContext {
  return {
    sessionID: "semantic-session",
    messageID: "message-id",
    agent: "test",
    directory,
    worktree: directory,
    abort: new AbortController().signal,
    metadata: () => {},
    ask: noopAsk,
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
  const bridgeCalls: BridgeCall[] = [];
  const bridge = {
    send: async (command: string, params: Record<string, unknown> = {}) => {
      sendCalls.push({ command, params });
      return await sendImpl(command, params);
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
    tools: semanticTools(createPluginContext(pool, config)),
  };
}

describe("semanticTools", () => {
  test("registers aft_search", () => {
    const { tools } = createMockSemanticHarness({}, () => ({ success: true }));

    expect(Object.keys(tools)).toEqual(["aft_search"]);
  });

  test("returns response.text and sends semantic_search params", async () => {
    const sdkCtx = createMockSdkContext("/tmp/project");
    const { bridgeCalls, sendCalls, tools } = createMockSemanticHarness({}, () => ({
      success: true,
      text: "src/auth.ts\nvalidateToken [function] lines 10-32 score 0.913",
    }));

    const output = await tools.aft_search.execute(
      { query: "authentication logic", topK: 5 },
      sdkCtx,
    );

    // Bridge now keyed by project root only; the session lives in params via callBridge helper.
    expect(bridgeCalls.length).toBe(1);
    expect(sendCalls).toEqual([
      {
        command: "semantic_search",
        params: {
          query: "authentication logic",
          top_k: 5,
          session_id: "semantic-session",
        },
      },
    ]);
    expect(output).toBe("src/auth.ts\nvalidateToken [function] lines 10-32 score 0.913");
  });

  test("returns semantic runtime errors as user-friendly text", async () => {
    const sdkCtx = createMockSdkContext("/tmp/project");
    const { tools } = createMockSemanticHarness({}, () => ({
      success: false,
      code: "semantic_search_unavailable",
      message: "Semantic search unavailable: ONNX Runtime not installed.",
    }));

    const output = await tools.aft_search.execute(
      { query: "authentication logic", topK: 5 },
      sdkCtx,
    );

    expect(output).toBe("Semantic search unavailable: ONNX Runtime not installed.");
  });
});
