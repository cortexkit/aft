/**
 * Unit tests for aft_callgraph argument shaping.
 */

/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { registerNavigateTool } from "../tools/navigate.js";
import { executeTool, makeMockApi, makeMockBridge, makePluginContext } from "./tool-test-utils.js";

async function expectRejectMessage(action: () => Promise<unknown>): Promise<string> {
  try {
    await action();
  } catch (error) {
    expect(error).toBeInstanceOf(Error);
    return (error as Error).message;
  }
  throw new Error("expected action to reject");
}

function toolArgs(call: { params: Record<string, unknown> }): Record<string, unknown> {
  return call.params.arguments as Record<string, unknown>;
}

describe("aft_callgraph adapter", () => {
  test("describes callgraph guidance without invalid read callgraph syntax", () => {
    const enabledApi = makeMockApi();
    const { bridge } = makeMockBridge();
    registerNavigateTool(enabledApi.api, makePluginContext(bridge));
    const enabled = enabledApi.tools.get("aft_callgraph")!.description;

    const disabledApi = makeMockApi();
    registerNavigateTool(
      disabledApi.api,
      makePluginContext(bridge, { config: { disabled_tools: ["aft_zoom"] } as never }),
    );
    const disabled = disabledApi.tools.get("aft_callgraph")!.description;
    expect(enabled).toContain("aft_zoom with `callgraph:true`");
    expect(disabled).toContain("Use aft_callgraph call_tree (depth 1)");
    expect(disabled).not.toContain("aft_zoom");
    expect(disabled).not.toContain("read with `callgraph:true`");
  });
  test("dispatches to the selected op and maps filePath to file", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, total_affected: 0 }));
    registerNavigateTool(api, makePluginContext(bridge));

    const result = (await executeTool(tools.get("aft_callgraph")!, {
      op: "impact",
      filePath: "src/app.ts",
      symbol: "run",
      depth: 4,
    })) as {
      content: Array<{ type: string; text: string }>;
      details?: Record<string, unknown>;
    };

    expect(result.content[0]?.text).toContain("affected call site");
    expect(result.content[0]?.text).not.toContain('"success"');
    expect(result.details).toMatchObject({ success: true, total_affected: 0 });
    expect(calls[0].command).toBe("tool_call");
    expect(calls[0].params.name).toBe("callgraph");
    expect(toolArgs(calls[0])).toEqual({
      op: "impact",
      filePath: "src/app.ts",
      symbol: "run",
      depth: 4,
    });
  });

  test("includeUnresolved is exposed and controls call_tree formatting only", async () => {
    const { api, tools } = makeMockApi();
    const payload = {
      success: true,
      name: "run",
      file: "src/app.ts",
      line: 1,
      children: [
        { name: "len", file: "src/app.ts", line: 2, resolved: false, children: [] },
        { name: "Some", file: "src/app.ts", line: 3, resolved: false, children: [] },
        { name: "project", file: "src/project.ts", line: 4, resolved: true, children: [] },
      ],
    };
    const { bridge, calls } = makeMockBridge(() => payload);
    registerNavigateTool(api, makePluginContext(bridge));
    const tool = tools.get("aft_callgraph")!;
    const schema = tool.parameters as {
      properties?: Record<string, { description?: string }>;
    };

    expect(schema.properties?.includeUnresolved).toBeDefined();
    expect(schema.properties?.includeUnresolved?.description).toContain("Defaults to false");
    expect(tool.description).toContain("includeUnresolved=true");

    const collapsed = (await executeTool(tool, {
      op: "call_tree",
      filePath: "src/app.ts",
      symbol: "run",
    })) as { content: Array<{ type: string; text: string }> };
    const expanded = (await executeTool(tool, {
      op: "call_tree",
      filePath: "src/app.ts",
      symbol: "run",
      includeUnresolved: true,
    })) as { content: Array<{ type: string; text: string }> };

    expect(collapsed.content[0]?.text).toContain("+ 2 unresolved external calls: len, Some");
    expect(collapsed.content[0]?.text).toContain("project [src/project.ts:4]");
    expect(collapsed.content[0]?.text).not.toContain("len [src/app.ts:2] [unresolved]");
    expect(expanded.content[0]?.text).toContain("len [src/app.ts:2] [unresolved]");
    expect(expanded.content[0]?.text).toContain("Some [src/app.ts:3] [unresolved]");
    expect(expanded.content[0]?.text).not.toContain("unresolved external calls");
    expect(calls).toHaveLength(2);
    expect(toolArgs(calls[0])).not.toHaveProperty("includeUnresolved");
    expect(toolArgs(calls[1]).includeUnresolved).toBe(true);
  });

  test("trace_data requires expression before bridge dispatch", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge();
    registerNavigateTool(api, makePluginContext(bridge));

    await expect(
      executeTool(tools.get("aft_callgraph")!, {
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

    await executeTool(tools.get("aft_callgraph")!, {
      op: "trace_data",
      filePath: "src/app.ts",
      symbol: "run",
      expression: "config.apiKey",
    });

    expect(calls[0].command).toBe("tool_call");
    expect(toolArgs(calls[0])).toMatchObject({ op: "trace_data", expression: "config.apiKey" });
  });

  test("trace_to_symbol requires and forwards target fields", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true }));
    registerNavigateTool(api, makePluginContext(bridge));

    await expect(
      executeTool(tools.get("aft_callgraph")!, {
        op: "trace_to_symbol",
        filePath: "src/app.ts",
        symbol: "run",
      }),
    ).rejects.toThrow("toSymbol");

    await executeTool(tools.get("aft_callgraph")!, {
      op: "trace_to_symbol",
      filePath: "src/app.ts",
      symbol: "run",
      toSymbol: "target",
      toFile: "src/target.ts",
      depth: 3,
    });

    expect(calls).toHaveLength(1);
    expect(calls[0].command).toBe("tool_call");
    expect(toolArgs(calls[0])).toMatchObject({
      op: "trace_to_symbol",
      toSymbol: "target",
      toFile: "src/target.ts",
      depth: 3,
    });
  });

  test("trace_to_symbol ambiguous_target errors include candidates (Rust top-level shape)", async () => {
    // Rust's error_with_data() merges extras into the top-level response,
    // so production traffic has `candidates` next to `code`/`message`, NOT
    // nested under `data`.
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge(() => ({
      success: false,
      code: "ambiguous_target",
      message: 'multiple symbols named "target"',
      candidates: [
        { file: "file1.rs", line: 42, symbol: "target" },
        { file: "file2.rs", line: 78, symbol: "target" },
      ],
    }));
    registerNavigateTool(api, makePluginContext(bridge));

    const message = await expectRejectMessage(() =>
      executeTool(tools.get("aft_callgraph")!, {
        op: "trace_to_symbol",
        filePath: "src/app.ts",
        symbol: "run",
        toSymbol: "target",
      }),
    );

    expect(message).toBe(
      'trace_to_symbol: ambiguous_target — multiple symbols named "target". Pass toFile to disambiguate:\n  - file1.rs:42\n  - file2.rs:78',
    );
  });

  test("trace_to_symbol ambiguous_target also works with nested data.candidates (forward compat)", async () => {
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge(() => ({
      success: false,
      code: "ambiguous_target",
      message: 'multiple symbols named "target"',
      data: {
        candidates: [{ file: "file1.rs", line: 42 }],
      },
    }));
    registerNavigateTool(api, makePluginContext(bridge));

    const message = await expectRejectMessage(() =>
      executeTool(tools.get("aft_callgraph")!, {
        op: "trace_to_symbol",
        filePath: "src/app.ts",
        symbol: "run",
        toSymbol: "target",
      }),
    );

    expect(message).toBe(
      'trace_to_symbol: ambiguous_target — multiple symbols named "target". Pass toFile to disambiguate:\n  - file1.rs:42',
    );
  });

  test("trace_to_symbol target_symbol_not_in_file lists alternate files", async () => {
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge(() => ({
      success: false,
      code: "target_symbol_not_in_file",
      message: "trace_to_symbol: target symbol 'foo' is not defined in toFile: wrong.rs",
      candidates: [
        { file: "right1.rs", line: 12 },
        { file: "right2.rs", line: 99 },
      ],
    }));
    registerNavigateTool(api, makePluginContext(bridge));

    const message = await expectRejectMessage(() =>
      executeTool(tools.get("aft_callgraph")!, {
        op: "trace_to_symbol",
        filePath: "src/app.ts",
        symbol: "run",
        toSymbol: "foo",
        toFile: "wrong.rs",
      }),
    );

    expect(message).toContain("target_symbol_not_in_file");
    expect(message).toContain("Try one of these files for toFile");
    expect(message).toContain("right1.rs:12");
    expect(message).toContain("right2.rs:99");
  });

  test("generic bridge errors keep code, message, and structured extras visible", async () => {
    // A genuine error code (not a soft negative) still throws so it renders as
    // an error, with code/message plus any top-level structured extras visible.
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge(() => ({
      success: false,
      code: "internal",
      message: "resolver crashed",
      file: "src/app.ts",
      symbol: "run",
    }));
    registerNavigateTool(api, makePluginContext(bridge));

    const message = await expectRejectMessage(() =>
      executeTool(tools.get("aft_callgraph")!, {
        op: "callers",
        filePath: "src/app.ts",
        symbol: "run",
      }),
    );

    expect(message).toContain("callers: internal — resolver crashed");
    expect(message).toContain('"file": "src/app.ts"');
    expect(message).toContain('"symbol": "run"');
  });

  test("read-only negatives (symbol_not_found, callgraph_building) return text, not an error", async () => {
    // These are legitimate "no result" / "retry shortly" answers from a
    // read-only query tool — they must NOT throw (which would render red in the
    // host UI). The agent still gets the full honest message as the result.
    for (const code of ["symbol_not_found", "callgraph_building"]) {
      const { api, tools } = makeMockApi();
      const { bridge } = makeMockBridge(() => ({
        success: false,
        code,
        message: `${code} happened`,
        file: "src/app.ts",
        symbol: "run",
      }));
      registerNavigateTool(api, makePluginContext(bridge));

      const result = (await executeTool(tools.get("aft_callgraph")!, {
        op: "callers",
        filePath: "src/app.ts",
        symbol: "run",
      })) as { content: Array<{ type: string; text: string }> };

      expect(result.content[0]?.text).toContain(`callers: ${code} — ${code} happened`);
    }
  });

  test("returns flat text and structured details for themed render", async () => {
    const { api, tools } = makeMockApi();
    const payload = {
      success: true,
      total_callers: 2,
      callers: [
        {
          file: "src/a.ts",
          callers: [
            { symbol: "fn", line: 10 },
            { symbol: "fn", line: 20 },
          ],
        },
      ],
    };
    const { bridge } = makeMockBridge(() => payload);
    registerNavigateTool(api, makePluginContext(bridge));

    const result = (await executeTool(tools.get("aft_callgraph")!, {
      op: "callers",
      filePath: "src/a.ts",
      symbol: "target",
    })) as {
      content: Array<{ type: string; text: string }>;
      details?: Record<string, unknown>;
    };

    expect(result.content[0]?.text).toContain("2 callers");
    expect(result.content[0]?.text).toContain("↳ fn:10, 20");
    expect(result.details).toEqual(payload);
  });
});
