/**
 * Unit tests for aft_import tool_call argument shaping.
 */

/// <reference path="../bun-test.d.ts" />

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { TObject } from "typebox";
import { Value } from "typebox/value";
import { registerImportTools } from "../tools/imports.js";
import {
  executeTool,
  makeExtContext,
  makeMockApi,
  makeMockBridge,
  makePluginContext,
} from "./tool-test-utils.js";

let projectRoot: string;

beforeAll(() => {
  projectRoot = mkdtempSync(join(tmpdir(), "aft-test-repo-"));
});

afterAll(() => {
  rmSync(projectRoot, { recursive: true, force: true });
});

describe("aft_import adapter", () => {
  test("registers validate as an optional string enum without TypeBox metadata", () => {
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge();
    registerImportTools(api, makePluginContext(bridge));

    const schema = tools.get("aft_import")!.parameters as TObject;
    expect(JSON.stringify(schema.properties.validate)).toBe(
      '{"type":"string","enum":["syntax","full"],"description":"Post-edit validation level (default: syntax)"}',
    );
    const args = { op: "organize", path: "src/app.ts" };
    expect(Value.Check(schema, args)).toBe(true);
    expect(Value.Check(schema, { ...args, validate: "syntax" })).toBe(true);
    expect(Value.Check(schema, { ...args, validate: "full" })).toBe(true);
    expect(Value.Check(schema, { ...args, validate: "none" })).toBe(false);
  });

  test("add forwards Pi camelCase args through aft_import tool_call", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({
      success: true,
      text: "added react",
      file: "src/app.ts",
    }));
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
      makeExtContext(projectRoot, "session-import"),
    );

    expect(calls[0].command).toBe("tool_call");
    expect(calls[0].params).toEqual({
      name: "import",
      arguments: {
        op: "add",
        filePath: "src/app.ts",
        module: "react",
        names: ["useMemo"],
        defaultImport: "React",
        typeOnly: true,
        validate: "full",
      },
      session_id: "session-import",
    });
  });

  test("remove forwards removeName without silently dropping it", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "removed react" }));
    registerImportTools(api, makePluginContext(bridge));

    await executeTool(tools.get("aft_import")!, {
      op: "remove",
      filePath: "src/app.ts",
      module: "react",
      removeName: "useEffect",
    });

    expect(calls[0].command).toBe("tool_call");
    expect(calls[0].params).toMatchObject({
      name: "import",
      arguments: {
        op: "remove",
        filePath: "src/app.ts",
        module: "react",
        removeName: "useEffect",
      },
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
