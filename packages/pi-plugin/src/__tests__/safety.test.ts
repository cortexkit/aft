/**
 * Unit tests for aft_safety argument shaping.
 */

/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { registerSafetyTool } from "../tools/safety.js";
import { executeTool, makeMockApi, makeMockBridge, makePluginContext } from "./tool-test-utils.js";

describe("aft_safety adapter", () => {
  test("checkpoint promotes filePath to files so a single-file checkpoint is not dropped", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true }));
    registerSafetyTool(api, makePluginContext(bridge));

    await executeTool(tools.get("aft_safety")!, {
      op: "checkpoint",
      name: "before-edit",
      filePath: "src/app.ts",
    });

    expect(calls[0].command).toBe("checkpoint");
    expect(calls[0].params).toMatchObject({ name: "before-edit", files: ["src/app.ts"] });
    expect(calls[0].params).not.toHaveProperty("file");
  });

  test("history requires filePath before bridge dispatch", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge();
    registerSafetyTool(api, makePluginContext(bridge));

    await expect(executeTool(tools.get("aft_safety")!, { op: "history" })).rejects.toThrow(
      "requires 'filePath'",
    );
    expect(calls).toHaveLength(0);
  });

  test("undo without filePath calls bridge without file param", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, operation: true }));
    registerSafetyTool(api, makePluginContext(bridge));

    await executeTool(tools.get("aft_safety")!, { op: "undo" });

    expect(calls[0].command).toBe("undo");
    expect(calls[0].params).toEqual({});
  });

  test("undo with filePath still passes file param", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, backup_id: "b1" }));
    registerSafetyTool(api, makePluginContext(bridge));

    await executeTool(tools.get("aft_safety")!, { op: "undo", filePath: "src/app.ts" });

    expect(calls[0].command).toBe("undo");
    expect(calls[0].params).toMatchObject({ file: "src/app.ts" });
  });

  test("restore maps to restore_checkpoint with name and optional file list", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true }));
    registerSafetyTool(api, makePluginContext(bridge));

    await executeTool(tools.get("aft_safety")!, {
      op: "restore",
      name: "before-edit",
      files: ["src/a.ts", "src/b.ts"],
    });

    expect(calls[0].command).toBe("restore_checkpoint");
    expect(calls[0].params).toMatchObject({
      name: "before-edit",
      files: ["src/a.ts", "src/b.ts"],
    });
  });
});
