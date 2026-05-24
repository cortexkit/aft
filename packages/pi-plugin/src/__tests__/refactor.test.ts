/**
 * Unit tests for aft_refactor argument shaping.
 */

/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { registerRefactorTool } from "../tools/refactor.js";
import { executeTool, makeMockApi, makeMockBridge, makePluginContext } from "./tool-test-utils.js";

describe("aft_refactor adapter", () => {
  test("extract converts inclusive Pi endLine to Rust-exclusive end_line", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true }));
    registerRefactorTool(api, makePluginContext(bridge));

    await executeTool(tools.get("aft_refactor")!, {
      op: "extract",
      filePath: "src/app.ts",
      name: "computeTotal",
      startLine: 10,
      endLine: 12,
    });

    expect(calls[0].command).toBe("extract_function");
    expect(calls[0].params).toMatchObject({
      file: "src/app.ts",
      name: "computeTotal",
      start_line: 10,
      end_line: 13,
    });
  });

  test("inline keeps callSiteLine and non-extract endLine unchanged", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true }));
    registerRefactorTool(api, makePluginContext(bridge));

    await executeTool(tools.get("aft_refactor")!, {
      op: "inline",
      filePath: "src/app.ts",
      symbol: "helper",
      callSiteLine: 44,
      endLine: 50,
    });

    expect(calls[0].command).toBe("inline_symbol");
    expect(calls[0].params).toMatchObject({
      file: "src/app.ts",
      symbol: "helper",
      call_site_line: 44,
      end_line: 50,
    });
  });

  test("move forwards symbol destination and disambiguating scope", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true }));
    registerRefactorTool(api, makePluginContext(bridge));

    await executeTool(tools.get("aft_refactor")!, {
      op: "move",
      filePath: "src/app.ts",
      symbol: "Service",
      destination: "src/service.ts",
      scope: "exports",
    });

    expect(calls[0].command).toBe("move_symbol");
    expect(calls[0].params).toMatchObject({
      file: "src/app.ts",
      symbol: "Service",
      destination: "src/service.ts",
      scope: "exports",
    });
  });
});
