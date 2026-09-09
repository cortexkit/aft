/**
 * Unit tests for aft_search argument shaping.
 */

/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import type { TSchema } from "typebox";
import { Value } from "typebox/value";
import { registerSemanticTool } from "../tools/semantic.js";
import { executeTool, makeMockApi, makeMockBridge, makePluginContext } from "./tool-test-utils.js";

function schemaAccepts(schema: unknown, value: unknown): boolean {
  return Value.Check(schema as TSchema, value);
}

function toolArgs(call: { params: Record<string, unknown> }): Record<string, unknown> {
  return call.params.arguments as Record<string, unknown>;
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

  test("topK schema accepts only bounded integers", () => {
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge();
    registerSemanticTool(api, makePluginContext(bridge));
    const schema = tools.get("aft_search")!.parameters;

    expect(schemaAccepts(schema, { query: "auth", topK: 1 })).toBe(true);
    expect(schemaAccepts(schema, { query: "auth", topK: 100 })).toBe(true);
    expect(schemaAccepts(schema, { query: "auth" })).toBe(true);
    expect(schemaAccepts(schema, { query: "auth", hint: "literal" })).toBe(true);
    expect(schemaAccepts(schema, { query: "auth", topK: 0 })).toBe(false);
    expect(schemaAccepts(schema, { query: "auth", topK: 101 })).toBe(false);
    expect(schemaAccepts(schema, { query: "auth", topK: 1.5 })).toBe(false);
    expect(schemaAccepts(schema, { query: "auth", topK: "10" })).toBe(false);
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
