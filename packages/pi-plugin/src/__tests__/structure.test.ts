/**
 * Unit tests for aft_transform argument shaping.
 */

/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { registerStructureTool } from "../tools/structure.js";
import { executeTool, makeMockApi, makeMockBridge, makePluginContext } from "./tool-test-utils.js";

describe("aft_transform adapter", () => {
  test("add_member maps container to Rust scope and preserves position", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true }));
    registerStructureTool(api, makePluginContext(bridge));

    await executeTool(tools.get("aft_transform")!, {
      op: "add_member",
      filePath: "src/app.ts",
      container: "Service",
      code: "dispose() {}",
      position: "last",
    });

    expect(calls[0].command).toBe("add_member");
    expect(calls[0].params).toMatchObject({
      file: "src/app.ts",
      scope: "Service",
      code: "dispose() {}",
      position: "last",
    });
  });

  test("wrap_try_catch maps catchBody and validate to Rust field names", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true }));
    registerStructureTool(api, makePluginContext(bridge));

    await executeTool(tools.get("aft_transform")!, {
      op: "wrap_try_catch",
      filePath: "src/app.ts",
      target: "run",
      catchBody: "throw error;",
      validate: "full",
    });

    expect(calls[0].command).toBe("wrap_try_catch");
    expect(calls[0].params).toMatchObject({
      file: "src/app.ts",
      target: "run",
      catch_body: "throw error;",
      validate: "full",
    });
  });

  test("add_struct_tags forwards tag tuple without dropping value", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true }));
    registerStructureTool(api, makePluginContext(bridge));

    await executeTool(tools.get("aft_transform")!, {
      op: "add_struct_tags",
      filePath: "main.go",
      target: "User",
      field: "Name",
      tag: "json",
      value: "name,omitempty",
    });

    expect(calls[0].params).toMatchObject({
      target: "User",
      field: "Name",
      tag: "json",
      value: "name,omitempty",
    });
  });
});
