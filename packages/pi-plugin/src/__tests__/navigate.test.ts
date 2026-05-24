/**
 * Unit tests for aft_navigate argument shaping.
 */

/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { registerNavigateTool } from "../tools/navigate.js";
import { executeTool, makeMockApi, makeMockBridge, makePluginContext } from "./tool-test-utils.js";

describe("aft_navigate adapter", () => {
  test("dispatches to the selected op and maps filePath to file", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true }));
    registerNavigateTool(api, makePluginContext(bridge));

    await executeTool(tools.get("aft_navigate")!, {
      op: "impact",
      filePath: "src/app.ts",
      symbol: "run",
      depth: 4,
    });

    expect(calls[0].command).toBe("impact");
    expect(calls[0].params).toEqual({
      op: "impact",
      file: "src/app.ts",
      symbol: "run",
      depth: 4,
    });
  });

  test("trace_data requires expression before bridge dispatch", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge();
    registerNavigateTool(api, makePluginContext(bridge));

    await expect(
      executeTool(tools.get("aft_navigate")!, {
        op: "trace_data",
        filePath: "src/app.ts",
        symbol: "run",
      }),
    ).rejects.toThrow("requires an `expression`");
    expect(calls).toHaveLength(0);
  });

  test("trace_data forwards expression when present", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true }));
    registerNavigateTool(api, makePluginContext(bridge));

    await executeTool(tools.get("aft_navigate")!, {
      op: "trace_data",
      filePath: "src/app.ts",
      symbol: "run",
      expression: "config.apiKey",
    });

    expect(calls[0].command).toBe("trace_data");
    expect(calls[0].params).toMatchObject({ expression: "config.apiKey" });
  });
});
