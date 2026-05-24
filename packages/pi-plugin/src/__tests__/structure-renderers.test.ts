/**
 * Renderer coverage for aft_transform.
 */

/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import {
  registerStructureTool,
  renderTransformCall,
  renderTransformResult,
} from "../tools/structure.js";
import { makeContext, makeResult, mockTheme, renderToString } from "./render-test-helpers.js";

function registeredTransformExecute() {
  let registered: { execute: (...args: unknown[]) => Promise<unknown> } | undefined;
  registerStructureTool(
    {
      registerTool(tool: { execute: (...args: unknown[]) => Promise<unknown> }) {
        registered = tool;
      },
    } as never,
    { pool: {} } as never,
  );
  if (!registered) throw new Error("aft_transform was not registered");
  const tool = registered;
  return (params: Record<string, unknown>) =>
    tool.execute("call-id", params, new AbortController().signal, () => {}, { cwd: "/repo" });
}

describe("transform renderer", () => {
  test("renderTransformCall shows op and target", () => {
    const output = renderToString(
      renderTransformCall(
        { op: "add_member", filePath: "src/a.ts", container: "Service" },
        mockTheme,
        makeContext({ op: "add_member", filePath: "src/a.ts", container: "Service" }),
      ),
    );
    expect(output).toContain("transform");
    expect(output).toContain("add_member");
    expect(output).toContain("Service");
  });

  test("renderTransformResult shows structured summary", () => {
    const output = renderToString(
      renderTransformResult(
        makeResult("", { file: "src/a.ts", scope: "Service" }),
        { op: "add_member", filePath: "src/a.ts", container: "Service" },
        mockTheme,
        makeContext({ op: "add_member", filePath: "src/a.ts", container: "Service" }),
      ),
    );
    expect(output).toContain("transformed add_member");
    expect(output).toContain("target Service");
  });

  test("renderTransformResult handles error paths", () => {
    const error = renderToString(
      renderTransformResult(
        makeResult("parse failed"),
        { op: "add_member", filePath: "src/a.ts", container: "Service" },
        mockTheme,
        makeContext(
          { op: "add_member", filePath: "src/a.ts", container: "Service" },
          { isError: true },
        ),
      ),
    );

    expect(error).toContain("parse failed");
  });
});

describe("transform validation", () => {
  test.each([
    [{ op: "add_member", filePath: "src/a.ts", code: "m() {}" }, "'container' is required"],
    [{ op: "add_member", filePath: "src/a.ts", container: "Service" }, "'code' is required"],
    [{ op: "add_derive", filePath: "src/lib.rs", derives: ["Debug"] }, "'target' is required"],
    [{ op: "add_derive", filePath: "src/lib.rs", target: "Foo" }, "'derives' array is required"],
    [{ op: "wrap_try_catch", filePath: "src/a.ts" }, "'target' is required"],
    [{ op: "add_decorator", filePath: "src/a.py", target: "Foo" }, "'decorator' is required"],
    [{ op: "add_struct_tags", filePath: "main.go", target: "User" }, "'field' is required"],
    [
      { op: "add_struct_tags", filePath: "main.go", target: "User", field: "Name" },
      "'tag' is required",
    ],
    [
      { op: "add_struct_tags", filePath: "main.go", target: "User", field: "Name", tag: "json" },
      "'value' is required",
    ],
  ])("rejects missing required params before bridge call", async (params, expected) => {
    const execute = registeredTransformExecute();

    await expect(execute(params)).rejects.toThrow(expected);
  });
});
