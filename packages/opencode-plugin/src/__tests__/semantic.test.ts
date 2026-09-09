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

function createMockSdkContext(
  directory = projectRoot,
  ask = noopAsk,
  sessionID = "semantic-session",
): ToolContext {
  return {
    sessionID,
    messageID: "message-id",
    agent: "test",
    directory,
    worktree: directory,
    abort: new AbortController().signal,
    metadata: () => {},
    ask,
  };
}

const DISCLOSURE = "index changed - order re-derived";
const DIRECT_RESULTS = Array.from({ length: 3200 }, (_, index) => `result-${index}`);

function directBackend(
  args: Record<string, unknown>,
  generation = "generation-1|nonce-a",
): BridgeResponse {
  const offset = args.offset ?? 0;
  const topK = args.topK ?? 10;
  if (typeof offset !== "number" || !Number.isInteger(offset) || offset < 0 || offset > 100000) {
    return {
      success: false,
      code: "invalid_request",
      text: "invalid_request: offset must be an integer between 0 and 100000",
    };
  }
  if (typeof topK !== "number" || !Number.isInteger(topK) || topK < 1 || topK > 100) {
    return {
      success: false,
      code: "invalid_request",
      text: "invalid_request: topK must be an integer between 1 and 100",
    };
  }
  const page = DIRECT_RESULTS.slice(offset, offset + topK);
  return {
    success: true,
    text: `page=${page.join(",")}\nsnapshot_generation=${generation}`,
    plan: { snapshot_generation: generation },
    results: page.map((file) => ({ file })),
  };
}

function disclosureCount(text: string): number {
  return text.split(DISCLOSURE).length - 1;
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

  test("forwards offset pages unchanged and matches direct backend paging", async () => {
    const sdkCtx = createMockSdkContext(projectRoot, noopAsk, "offset-session");
    const { toolCallCalls, tools } = createMockSemanticHarness({}, (_command, args) =>
      directBackend(args),
    );

    for (const request of [
      { query: "paged query", offset: 5, topK: 10 },
      { query: "paged query", offset: 3200, topK: 10 },
    ]) {
      const expected = directBackend(request);
      const output = await tools.aft_search.execute(request, sdkCtx);
      const forwarded = toolCallCalls.at(-1)?.rawArgs;

      expect(forwarded?.offset).toBe(request.offset);
      expect(forwarded).toEqual(request);
      expect(output).toBe(expected.text);
    }
  });

  test("generation continuity is isolated by session and compares opaque tokens by equality", async () => {
    const generations = [
      "index-7|nonce-a",
      "index-7|nonce-a",
      "index-7|nonce-b",
      "index-7|nonce-b",
      "index-7|nonce-a",
    ];
    const { tools } = createMockSemanticHarness({}, (_command, args) =>
      directBackend(args, generations.shift() ?? "unexpected"),
    );
    const sessionA = createMockSdkContext(projectRoot, noopAsk, "continuity-a");
    const sessionB = createMockSdkContext(projectRoot, noopAsk, "continuity-b");

    const firstA = await tools.aft_search.execute(
      { query: "same query", offset: 0, topK: 10 },
      sessionA,
    );
    const firstB = await tools.aft_search.execute(
      { query: "same query", offset: 0, topK: 10 },
      sessionB,
    );
    const changedA = await tools.aft_search.execute(
      { query: "same query", offset: 10, topK: 10 },
      sessionA,
    );
    const stableA = await tools.aft_search.execute(
      { query: "same query", offset: 20, topK: 10 },
      sessionA,
    );
    const stableB = await tools.aft_search.execute(
      { query: "same query", offset: 10, topK: 10 },
      sessionB,
    );

    expect(disclosureCount(firstA)).toBe(0);
    expect(disclosureCount(firstB)).toBe(0);
    expect(disclosureCount(changedA)).toBe(1);
    expect(changedA.startsWith(`${DISCLOSURE}\n`)).toBe(true);
    expect(disclosureCount(stableA)).toBe(0);
    expect(disclosureCount(stableB)).toBe(0);
  });

  test("first high-offset request and interleaved queries do not cross-trigger continuity", async () => {
    const { tools } = createMockSemanticHarness({}, (_command, args) => {
      const normalized = String(args.query).trim().replace(/\s+/gu, " ").toLowerCase();
      const generation = normalized === "query one" ? "generation-one" : "generation-two";
      return directBackend(args, generation);
    });
    const directSession = createMockSdkContext(projectRoot, noopAsk, "direct-offset");
    const interleavedSession = createMockSdkContext(projectRoot, noopAsk, "interleaved");

    const directReply = await tools.aft_search.execute(
      { query: "query one", offset: 200, topK: 10 },
      directSession,
    );
    const rawReply = directBackend({ query: "query one", offset: 200, topK: 10 });
    expect(directReply).toContain("snapshot_generation=generation-one");
    expect(disclosureCount(directReply)).toBe(0);
    expect(disclosureCount(String(rawReply.text))).toBe(0);

    const replies = [];
    replies.push(await tools.aft_search.execute({ query: "query one" }, interleavedSession));
    replies.push(await tools.aft_search.execute({ query: "query two" }, interleavedSession));
    replies.push(await tools.aft_search.execute({ query: "  QUERY   one  " }, interleavedSession));
    expect(replies.every((reply) => disclosureCount(reply) === 0)).toBe(true);
  });

  test("project roots isolate continuity observations", async () => {
    let generation = "root-one-generation";
    const { tools } = createMockSemanticHarness({}, (_command, args) =>
      directBackend(args, generation),
    );
    const session = createMockSdkContext(projectRoot, noopAsk, "root-session");

    const first = await tools.aft_search.execute({ query: "root query" }, session);
    generation = "root-two-generation";
    const second = await tools.aft_search.execute(
      { query: "root query", path: process.env.TMPDIR ?? "/tmp" },
      session,
    );

    expect(disclosureCount(first)).toBe(0);
    expect(disclosureCount(second)).toBe(0);
  });

  test("continuity survives backend restart and discloses only the first new-generation page", async () => {
    let restarted = false;
    const { tools } = createMockSemanticHarness({}, (_command, args) => {
      if (!restarted) return directBackend(args, "index-12|process-nonce-a");
      return {
        ...directBackend(args, "index-12|process-nonce-b"),
        text: `restarted-${String(args.offset ?? 0)}\nsnapshot_generation=index-12|process-nonce-b`,
      };
    });
    const session = createMockSdkContext(projectRoot, noopAsk, "restart-session");

    await tools.aft_search.execute({ query: "restart query", offset: 0 }, session);
    restarted = true;
    const firstAfterRestart = await tools.aft_search.execute(
      { query: "restart query", offset: 10 },
      session,
    );
    const secondAfterRestart = await tools.aft_search.execute(
      { query: "restart query", offset: 20 },
      session,
    );

    expect(firstAfterRestart).toBe(
      `${DISCLOSURE}\nrestarted-10\nsnapshot_generation=index-12|process-nonce-b`,
    );
    expect(secondAfterRestart).toBe("restarted-20\nsnapshot_generation=index-12|process-nonce-b");
  });

  test("offset and topK surface schemas preserve their public domains", async () => {
    const { tools } = createMockSemanticHarness({}, () => ({ success: true, text: "ok" }));
    const args = tools.aft_search.args as Record<
      string,
      { safeParse: (value: unknown) => { success: boolean } }
    >;

    for (const value of [0, 5, 3200, 100000]) {
      expect(args.offset.safeParse(value).success).toBe(true);
    }
    for (const value of [-1, 1.5, 100001, "5"]) {
      expect(args.offset.safeParse(value).success).toBe(false);
    }
    expect(args.topK.safeParse(1).success).toBe(true);
    expect(args.topK.safeParse(100).success).toBe(true);
    expect(args.topK.safeParse(0).success).toBe(false);
    expect(args.topK.safeParse(300).success).toBe(false);
    expect(args.path.safeParse(projectRoot).success).toBe(true);
    expect(tools.aft_search.description.split("Use `offset`").length - 1).toBe(1);

    const sdkCtx = createMockSdkContext(projectRoot, noopAsk, "invalid-offsets");
    await expect(tools.aft_search.execute({ query: "q", offset: -1 }, sdkCtx)).rejects.toThrow(
      "offset must be between 0 and 100000",
    );
    await expect(tools.aft_search.execute({ query: "q", offset: 1.5 }, sdkCtx)).rejects.toThrow(
      "offset must be an integer between 0 and 100000",
    );
    await expect(tools.aft_search.execute({ query: "q", offset: 100001 }, sdkCtx)).rejects.toThrow(
      "offset must be between 0 and 100000",
    );
    await expect(tools.aft_search.execute({ query: "q", topK: 300 }, sdkCtx)).rejects.toThrow(
      "topK must be between 1 and 100",
    );
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
