/**
 * Unit tests for aft_search argument shaping.
 */

/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import type { TSchema } from "typebox";
import { Value } from "typebox/value";
import { registerSemanticTool } from "../tools/semantic.js";
import {
  executeTool,
  makeExtContext,
  makeMockApi,
  makeMockBridge,
  makePluginContext,
} from "./tool-test-utils.js";

function schemaAccepts(schema: unknown, value: unknown): boolean {
  return Value.Check(schema as TSchema, value);
}

function toolArgs(call: { params: Record<string, unknown> }): Record<string, unknown> {
  return call.params.arguments as Record<string, unknown>;
}

const DISCLOSURE = "index changed - order re-derived";
const DIRECT_RESULTS = Array.from({ length: 3200 }, (_, index) => `result-${index}`);

function directBackend(
  args: Record<string, unknown>,
  generation = "generation-1|nonce-a",
): Record<string, unknown> {
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

function resultText(result: unknown): string {
  return (result as { content: Array<{ text: string }> }).content[0]?.text ?? "";
}

function disclosureCount(text: string): number {
  return text.split(DISCLOSURE).length - 1;
}

describe("aft_search adapter", () => {
  test("maps topK while ignoring a legacy hint and carries structured details", async () => {
    const { api, tools } = makeMockApi();
    const bridgeResponse = {
      success: true,
      text: "ready results",
      interpreted_as: "hybrid",
      semantic_status: "ready",
      more_available: true,
      engine_capped: false,
      fully_degraded: false,
      warnings: ["short_query_rerouted"],
      results: [{ file: "src/search.ts", kind: "function", source: "semantic" }],
    };
    const { bridge, calls } = makeMockBridge(() => bridgeResponse);
    registerSemanticTool(api, makePluginContext(bridge));

    const result = (await executeTool(tools.get("aft_search")!, {
      query: "retry logic",
      topK: 7,
      hint: "literal",
      includeTests: true,
    })) as { content: Array<{ text: string }>; details: Record<string, unknown> };

    expect(calls[0].command).toBe("tool_call");
    expect(calls[0].params.name).toBe("search");
    expect(toolArgs(calls[0])).toEqual({
      query: "retry logic",
      topK: 7,
      includeTests: true,
    });
    expect(result.content[0].text).toBe("ready results");
    expect(result.details.interpreted_as).toBe("hybrid");
    expect(result.details.semantic_status).toBe("ready");
    expect(result.details.more_available).toBe(true);
    expect(result.details.engine_capped).toBe(false);
    expect(result.details.fully_degraded).toBe(false);
    expect(result.details.warnings).toEqual(["short_query_rerouted"]);
    const detailsResults = result.details.results as Array<Record<string, unknown>>;
    expect(detailsResults[0].source).toBe("semantic");
  });

  test("forwards offset pages unchanged and matches direct backend paging", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge((_command, args) => directBackend(args));
    registerSemanticTool(api, makePluginContext(bridge));
    const tool = tools.get("aft_search")!;
    const session = makeExtContext(process.cwd(), "offset-session");

    for (const request of [
      { query: "paged query", offset: 5, topK: 10 },
      { query: "paged query", offset: 3200, topK: 10 },
    ]) {
      const expected = directBackend(request);
      const result = await executeTool(tool, request, session);
      const forwarded = toolArgs(calls.at(-1)!);

      expect(forwarded.offset).toBe(request.offset);
      expect(forwarded).toEqual(request);
      expect(resultText(result)).toBe(expected.text);
    }
  });

  test("generation continuity is isolated by session, query, and project root", async () => {
    const { api, tools } = makeMockApi();
    const generations = [
      "index-7|nonce-a",
      "index-7|nonce-a",
      "index-7|nonce-b",
      "index-7|nonce-b",
      "index-7|nonce-a",
    ];
    const { bridge } = makeMockBridge((_command, args) =>
      directBackend(args, generations.shift() ?? "unexpected"),
    );
    registerSemanticTool(api, makePluginContext(bridge));
    const tool = tools.get("aft_search")!;
    const sessionA = makeExtContext(process.cwd(), "continuity-a");
    const sessionB = makeExtContext(process.cwd(), "continuity-b");

    const firstA = resultText(
      await executeTool(tool, { query: "same query", offset: 0, topK: 10 }, sessionA),
    );
    const firstB = resultText(
      await executeTool(tool, { query: "same query", offset: 0, topK: 10 }, sessionB),
    );
    const changedA = resultText(
      await executeTool(tool, { query: "same query", offset: 10, topK: 10 }, sessionA),
    );
    const stableA = resultText(
      await executeTool(tool, { query: "same query", offset: 20, topK: 10 }, sessionA),
    );
    const stableB = resultText(
      await executeTool(tool, { query: "same query", offset: 10, topK: 10 }, sessionB),
    );

    expect(disclosureCount(firstA)).toBe(0);
    expect(disclosureCount(firstB)).toBe(0);
    expect(disclosureCount(changedA)).toBe(1);
    expect(changedA.startsWith(`${DISCLOSURE}\n`)).toBe(true);
    expect(disclosureCount(stableA)).toBe(0);
    expect(disclosureCount(stableB)).toBe(0);
  });

  test("first high-offset request and interleaved queries do not cross-trigger continuity", async () => {
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge((_command, args) => {
      const normalized = String(args.query).trim().replace(/\s+/gu, " ").toLowerCase();
      const generation = normalized === "query one" ? "generation-one" : "generation-two";
      return directBackend(args, generation);
    });
    registerSemanticTool(api, makePluginContext(bridge));
    const tool = tools.get("aft_search")!;
    const directSession = makeExtContext(process.cwd(), "direct-offset");
    const interleavedSession = makeExtContext(process.cwd(), "interleaved");

    const directReply = resultText(
      await executeTool(tool, { query: "query one", offset: 200, topK: 10 }, directSession),
    );
    const rawReply = directBackend({ query: "query one", offset: 200, topK: 10 });
    expect(directReply).toContain("snapshot_generation=generation-one");
    expect(disclosureCount(directReply)).toBe(0);
    expect(disclosureCount(String(rawReply.text))).toBe(0);

    const replies = [];
    replies.push(resultText(await executeTool(tool, { query: "query one" }, interleavedSession)));
    replies.push(resultText(await executeTool(tool, { query: "query two" }, interleavedSession)));
    replies.push(
      resultText(await executeTool(tool, { query: "  QUERY   one  " }, interleavedSession)),
    );
    expect(replies.every((reply) => disclosureCount(reply) === 0)).toBe(true);
  });

  test("project roots isolate continuity observations", async () => {
    const { api, tools } = makeMockApi();
    let generation = "root-one-generation";
    const { bridge } = makeMockBridge((_command, args) => directBackend(args, generation));
    registerSemanticTool(api, makePluginContext(bridge));
    const tool = tools.get("aft_search")!;
    const otherRoot = process.env.TMPDIR ?? "/tmp";

    const first = resultText(
      await executeTool(
        tool,
        { query: "root query" },
        makeExtContext(process.cwd(), "root-session"),
      ),
    );
    generation = "root-two-generation";
    const second = resultText(
      await executeTool(
        tool,
        { query: "root query", path: otherRoot },
        makeExtContext(process.cwd(), "root-session"),
      ),
    );

    expect(disclosureCount(first)).toBe(0);
    expect(disclosureCount(second)).toBe(0);
  });

  test("continuity survives bridge restart and discloses only the first new-generation page", async () => {
    const { api, tools } = makeMockApi();
    const initial = makeMockBridge((_command, args) =>
      directBackend(args, "index-12|process-nonce-a"),
    );
    const restarted = makeMockBridge((_command, args) => ({
      ...directBackend(args, "index-12|process-nonce-b"),
      text: `restarted-${String(args.offset ?? 0)}\nsnapshot_generation=index-12|process-nonce-b`,
    }));
    let activeBridge = initial.bridge;
    const context = makePluginContext(initial.bridge, {
      pool: { getBridge: () => activeBridge } as never,
    });
    registerSemanticTool(api, context);
    const tool = tools.get("aft_search")!;
    const session = makeExtContext(process.cwd(), "restart-session");

    await executeTool(tool, { query: "restart query", offset: 0 }, session);
    activeBridge = restarted.bridge;
    const firstAfterRestart = resultText(
      await executeTool(tool, { query: "restart query", offset: 10 }, session),
    );
    const secondAfterRestart = resultText(
      await executeTool(tool, { query: "restart query", offset: 20 }, session),
    );

    expect(firstAfterRestart).toBe(
      `${DISCLOSURE}\nrestarted-10\nsnapshot_generation=index-12|process-nonce-b`,
    );
    expect(secondAfterRestart).toBe("restarted-20\nsnapshot_generation=index-12|process-nonce-b");
  });

  test("forwards the host abort signal to standalone search transport", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "ok" }));
    registerSemanticTool(api, makePluginContext(bridge));
    const controller = new AbortController();

    await executeTool(
      tools.get("aft_search")!,
      { query: "slow embedding" },
      undefined,
      controller.signal,
    );

    expect(calls[0].options).toMatchObject({ abortSignal: controller.signal });
  });

  test("throws bridge failure envelopes so Pi renders them through its error path", async () => {
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge(() => ({
      success: false,
      code: "semantic_search_unavailable",
      message: "Semantic search unavailable: ONNX Runtime not installed.",
    }));
    registerSemanticTool(api, makePluginContext(bridge));

    await expect(executeTool(tools.get("aft_search")!, { query: "retry logic" })).rejects.toThrow(
      "Semantic search unavailable",
    );
  });

  test("omits top_k when topK is not provided to preserve Rust defaults", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "ok" }));
    registerSemanticTool(api, makePluginContext(bridge));

    await executeTool(tools.get("aft_search")!, { query: "auth flow" });

    expect(toolArgs(calls[0])).toEqual({ query: "auth flow" });
  });

  test("rejects blank queries before bridge calls", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "should not call" }));
    registerSemanticTool(api, makePluginContext(bridge));

    await expect(executeTool(tools.get("aft_search")!, { query: "   " })).rejects.toThrow(
      "invalid params",
    );

    expect(calls).toEqual([]);
  });

  test("schema does not advertise the legacy hint property", () => {
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge();
    registerSemanticTool(api, makePluginContext(bridge));
    const schema = tools.get("aft_search")!.parameters as { properties?: Record<string, unknown> };

    expect(schema.properties?.hint).toBeUndefined();
  });

  test("topK and offset schemas accept only their unchanged public domains", () => {
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge();
    registerSemanticTool(api, makePluginContext(bridge));
    const tool = tools.get("aft_search")!;
    const schema = tool.parameters;

    expect(schemaAccepts(schema, { query: "auth", topK: 1 })).toBe(true);
    expect(schemaAccepts(schema, { query: "auth", topK: 100 })).toBe(true);
    expect(schemaAccepts(schema, { query: "auth", offset: 0 })).toBe(true);
    expect(schemaAccepts(schema, { query: "auth", offset: 100000 })).toBe(true);
    expect(schemaAccepts(schema, { query: "auth" })).toBe(true);
    expect(schemaAccepts(schema, { query: "auth", hint: "literal" })).toBe(true);
    expect(schemaAccepts(schema, { query: "auth", topK: 0 })).toBe(false);
    expect(schemaAccepts(schema, { query: "auth", topK: 101 })).toBe(false);
    expect(schemaAccepts(schema, { query: "auth", topK: 1.5 })).toBe(false);
    expect(schemaAccepts(schema, { query: "auth", topK: "10" })).toBe(false);
    expect(schemaAccepts(schema, { query: "auth", offset: -1 })).toBe(false);
    expect(schemaAccepts(schema, { query: "auth", offset: 100001 })).toBe(false);
    expect(schemaAccepts(schema, { query: "auth", offset: 1.5 })).toBe(false);
    expect(schemaAccepts(schema, { query: "auth", offset: "5" })).toBe(false);

    const properties = (schema as { properties: Record<string, Record<string, unknown>> })
      .properties;
    expect(properties.topK).toMatchObject({ type: "integer", minimum: 1, maximum: 100 });
    expect(properties.offset).toMatchObject({ type: "integer", minimum: 0, maximum: 100000 });
    expect(properties.path.description).toContain("different Git project");
    expect(properties.path.description).toContain("not a subdirectory filter");
    expect(tool.description.split("Use `offset`").length - 1).toBe(1);
  });

  test("includeTests schema accepts booleans only", () => {
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge();
    registerSemanticTool(api, makePluginContext(bridge));
    const schema = tools.get("aft_search")!.parameters;

    expect(schemaAccepts(schema, { query: "auth", includeTests: true })).toBe(true);
    expect(schemaAccepts(schema, { query: "auth", includeTests: false })).toBe(true);
    expect(schemaAccepts(schema, { query: "auth", includeTests: "true" })).toBe(false);
  });
});
