/**
 * Unit tests for lsp_diagnostics argument shaping.
 */

/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { registerLspTools } from "../tools/lsp.js";
import { executeTool, makeMockApi, makeMockBridge, makePluginContext } from "./tool-test-utils.js";

describe("lsp_diagnostics adapter", () => {
  test("maps filePath, severity, and waitMs to Rust request names", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, diagnostics: [] }));
    registerLspTools(api, makePluginContext(bridge));

    await executeTool(tools.get("lsp_diagnostics")!, {
      filePath: "src/app.ts",
      severity: "warning",
      waitMs: 250,
    });

    expect(calls[0].command).toBe("lsp_diagnostics");
    expect(calls[0].params).toEqual({
      file: "src/app.ts",
      severity: "warning",
      wait_ms: 250,
    });
  });

  test("maps directory mode without also sending file", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, diagnostics: [] }));
    registerLspTools(api, makePluginContext(bridge));

    await executeTool(tools.get("lsp_diagnostics")!, { directory: "src", severity: "all" });

    expect(calls[0].params).toEqual({ directory: "src", severity: "all" });
  });

  test("rejects mutually exclusive filePath and directory before bridge dispatch", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge();
    registerLspTools(api, makePluginContext(bridge));

    await expect(
      executeTool(tools.get("lsp_diagnostics")!, { filePath: "a.ts", directory: "src" }),
    ).rejects.toThrow("mutually exclusive");
    expect(calls).toHaveLength(0);
  });
});
