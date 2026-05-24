/**
 * Unit tests for aft_import argument shaping.
 */

/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { registerImportTools } from "../tools/imports.js";
import {
  executeTool,
  makeExtContext,
  makeMockApi,
  makeMockBridge,
  makePluginContext,
} from "./tool-test-utils.js";

describe("aft_import adapter", () => {
  test("add maps Pi camelCase args to Rust snake_case request fields", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, file: "src/app.ts" }));
    registerImportTools(api, makePluginContext(bridge));

    await executeTool(
      tools.get("aft_import")!,
      {
        op: "add",
        filePath: "src/app.ts",
        module: "react",
        names: ["useMemo"],
        defaultImport: "React",
        typeOnly: true,
        validate: "full",
      },
      makeExtContext("/repo", "session-import"),
    );

    expect(calls[0].command).toBe("add_import");
    expect(calls[0].params).toEqual({
      file: "src/app.ts",
      module: "react",
      names: ["useMemo"],
      default_import: "React",
      type_only: true,
      validate: "full",
      session_id: "session-import",
    });
  });

  test("remove maps removeName to name instead of silently dropping it", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true }));
    registerImportTools(api, makePluginContext(bridge));

    await executeTool(tools.get("aft_import")!, {
      op: "remove",
      filePath: "src/app.ts",
      module: "react",
      removeName: "useEffect",
    });

    expect(calls[0].command).toBe("remove_import");
    expect(calls[0].params).toMatchObject({
      file: "src/app.ts",
      module: "react",
      name: "useEffect",
    });
  });

  test("add/remove reject missing module before calling the bridge", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge();
    registerImportTools(api, makePluginContext(bridge));

    await expect(
      executeTool(tools.get("aft_import")!, { op: "add", filePath: "src/app.ts" }),
    ).rejects.toThrow("requires 'module'");
    expect(calls).toHaveLength(0);
  });
});
