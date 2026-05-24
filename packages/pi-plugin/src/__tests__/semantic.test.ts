/**
 * Unit tests for aft_search argument shaping.
 */

/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { registerSemanticTool } from "../tools/semantic.js";
import { executeTool, makeMockApi, makeMockBridge, makePluginContext } from "./tool-test-utils.js";

describe("aft_search adapter", () => {
  test("maps topK to top_k and surfaces bridge text directly", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "ready results" }));
    registerSemanticTool(api, makePluginContext(bridge));

    const result = (await executeTool(tools.get("aft_search")!, {
      query: "retry logic",
      topK: 7,
    })) as { content: Array<{ text: string }> };

    expect(calls[0].command).toBe("semantic_search");
    expect(calls[0].params).toEqual({ query: "retry logic", top_k: 7 });
    expect(result.content[0].text).toBe("ready results");
  });

  test("omits top_k when topK is not provided to preserve Rust defaults", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true }));
    registerSemanticTool(api, makePluginContext(bridge));

    await executeTool(tools.get("aft_search")!, { query: "auth flow" });

    expect(calls[0].params).toEqual({ query: "auth flow" });
  });
});
