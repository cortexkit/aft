/**
 * Unit tests for aft_conflicts bridge dispatch.
 */

/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { registerConflictsTool } from "../tools/conflicts.js";
import { executeTool, makeMockApi, makeMockBridge, makePluginContext } from "./tool-test-utils.js";

describe("aft_conflicts adapter", () => {
  test("calls git_conflicts with an empty request and returns bridge text", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "No conflicts" }));
    registerConflictsTool(api, makePluginContext(bridge));

    const result = (await executeTool(tools.get("aft_conflicts")!, {})) as {
      content: Array<{ text: string }>;
    };

    expect(calls[0]).toMatchObject({ command: "git_conflicts", params: {} });
    expect(result.content[0].text).toBe("No conflicts");
  });

  test("falls back to JSON text when bridge omits formatted text", async () => {
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge(() => ({ success: true, conflicts: [] }));
    registerConflictsTool(api, makePluginContext(bridge));

    const result = (await executeTool(tools.get("aft_conflicts")!, {})) as {
      content: Array<{ text: string }>;
    };

    expect(result.content[0].text).toContain('"conflicts": []');
  });
});
