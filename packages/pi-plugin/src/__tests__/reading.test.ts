/**
 * Unit tests for aft_outline/aft_zoom argument shaping.
 */

/// <reference path="../bun-test.d.ts" />

import { afterEach, describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { registerReadingTools } from "../tools/reading.js";
import { executeTool, makeMockApi, makeMockBridge, makePluginContext } from "./tool-test-utils.js";

const tempRoots: string[] = [];

function toolArgs(call: { params: Record<string, unknown> }): Record<string, unknown> {
  return call.params.arguments as Record<string, unknown>;
}

async function tempProject(): Promise<string> {
  const root = await mkdtemp(join(tmpdir(), "aft-pi-reading-"));
  tempRoots.push(root);
  return root;
}

afterEach(async () => {
  await Promise.all(tempRoots.splice(0).map((root) => rm(root, { recursive: true, force: true })));
});

describe("reading tool adapters", () => {
  test("describes outline guidance without invalid read callgraph syntax", () => {
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge();
    registerReadingTools(api, makePluginContext(bridge), { outline: true, zoom: true });
    const enabled = tools.get("aft_outline")!.description;

    const disabledApi = makeMockApi();
    registerReadingTools(
      disabledApi.api,
      makePluginContext(bridge, { config: { disabled_tools: ["aft_zoom"] } as never }),
      { outline: true, zoom: true },
    );
    const disabled = disabledApi.tools.get("aft_outline")!.description;
    expect(enabled).toContain("aft_zoom with `callgraph:true`");
    expect(enabled).toContain("breadth-first with directory rollups");
    expect(enabled).toContain("line count");
    expect(enabled).not.toContain("up to 200 files");
    expect(enabled).not.toContain("byte metadata");
    expect(disabled).toContain("prefer aft_search + read on named symbols");
    expect(disabled).not.toContain("aft_zoom");
    expect(disabled).not.toContain("read with `callgraph:true`");
  });
  test("aft_outline maps a target array to the bridge files request", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "outline" }));
    registerReadingTools(api, makePluginContext(bridge), { outline: true, zoom: true });

    await executeTool(tools.get("aft_outline")!, { target: ["src/a.ts", "src/b.ts"] });

    expect(calls[0].command).toBe("tool_call");
    expect(calls[0].params.name).toBe("outline");
    expect(toolArgs(calls[0])).toEqual({ target: ["src/a.ts", "src/b.ts"] });
  });

  test("aft_outline detects directories and sends an absolute directory path", async () => {
    const root = await tempProject();
    await mkdir(join(root, "src"));
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "files" }));
    registerReadingTools(api, makePluginContext(bridge), { outline: true, zoom: false });

    await executeTool(tools.get("aft_outline")!, { target: "src" }, { cwd: root } as never);

    expect(calls[0].command).toBe("tool_call");
    expect(toolArgs(calls[0])).toEqual({ target: "src" });
  });

  test("aft_outline treats missing local paths as file targets and lets Rust report errors", async () => {
    const root = await tempProject();
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "missing" }));
    registerReadingTools(api, makePluginContext(bridge), { outline: true, zoom: false });

    await executeTool(tools.get("aft_outline")!, { target: "src/missing.ts" }, {
      cwd: root,
    } as never);

    expect(calls[0].params.name).toBe("outline");
    expect(toolArgs(calls[0])).toEqual({ target: "src/missing.ts" });
  });

  test("aft_outline files:true appends a walk-cap footer after the file table", async () => {
    const root = await tempProject();
    await mkdir(join(root, "src"));
    const uncheckedFiles = Array.from({ length: 12 }, (_, index) => `src/overflow-${index + 1}.ts`);
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({
      success: true,
      text: [
        "path | language | symbols",
        "",
        "⚠ Partial result: walk truncated at 200 files. 12 additional files in this directory were not indexed.",
        "Unchecked files:",
        ...uncheckedFiles.slice(0, 10).map((file) => `  ${file}`),
        "  ... +2 more",
      ].join("\n"),
      complete: false,
      walk_truncated: true,
      unchecked_files: uncheckedFiles,
    }));
    registerReadingTools(api, makePluginContext(bridge), { outline: true, zoom: false });

    const result = (await executeTool(tools.get("aft_outline")!, { target: "src", files: true }, {
      cwd: root,
    } as never)) as { content: Array<{ text: string }> };

    expect(toolArgs(calls[0])).toEqual({ target: "src", files: true });
    expect(result.content[0].text).toContain("path | language | symbols");
    expect(result.content[0].text).toContain(
      "⚠ Partial result: walk truncated at 200 files. 12 additional files in this directory were not indexed.",
    );
    expect(result.content[0].text).toContain("Unchecked files:");
    expect(result.content[0].text).toContain("src/overflow-1.ts");
    expect(result.content[0].text).toContain("src/overflow-10.ts");
    expect(result.content[0].text).not.toContain("src/overflow-11.ts");
    expect(result.content[0].text).toContain("... +2 more");
  });

  test("aft_outline unwraps JSON envelope string (including whitespace-padded) in response.text", async () => {
    const root = await tempProject();
    await mkdir(join(root, "src"));
    const envelope = JSON.stringify(
      {
        success: true,
        text: "src/\n  index.ts (ts)\n",
      },
      null,
      2,
    );
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge(() => ({
      success: true,
      text: `\n${envelope}\n`,
    }));
    registerReadingTools(api, makePluginContext(bridge), { outline: true, zoom: false });

    const result = (await executeTool(tools.get("aft_outline")!, { target: "src" }, {
      cwd: root,
    } as never)) as { content: Array<{ text: string }> };

    expect(result.content[0].text).toBe("src/\n  index.ts (ts)\n");
  });

  test("aft_zoom maps contextLines to one tool_call and preserves server partial text", async () => {
    const root = await tempProject();
    await writeFile(join(root, "src.ts"), "export function ok() {}\n");
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({
      success: true,
      text: [
        "Incomplete zoom results: one or more symbols failed.",
        "",
        "src.ts:1-1 [function ok]",
        "",
        "1: export function ok() {}",
        "",
        'Symbol "missing" not found: not found',
      ].join("\n"),
      complete: false,
      symbols: [
        {
          name: "ok",
          response: {
            success: true,
            name: "ok",
            kind: "function",
            range: { start_line: 1, end_line: 1 },
            content: "export function ok() {}",
          },
        },
        { name: "missing", response: { success: false, message: "not found" } },
      ],
    }));
    registerReadingTools(api, makePluginContext(bridge), { outline: true, zoom: true });

    const result = (await executeTool(
      tools.get("aft_zoom")!,
      {
        filePath: "src.ts",
        symbols: ["ok", "missing"],
        contextLines: 2,
      },
      { cwd: root } as never,
    )) as { content: Array<{ text: string }> };

    expect(calls).toHaveLength(1);
    expect(calls[0].command).toBe("tool_call");
    expect(calls[0].params.name).toBe("zoom");
    expect(toolArgs(calls[0])).toEqual({
      filePath: "src.ts",
      symbols: ["ok", "missing"],
      contextLines: 2,
    });
    expect(result.content[0].text).toContain("src.ts:1-1");
    expect(result.content[0].text).toContain('Symbol "missing" not found: not found');
  });

  test("aft_zoom rejects ambiguous filePath/url input before bridge dispatch", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge();
    registerReadingTools(api, makePluginContext(bridge), { outline: false, zoom: true });

    await expect(
      executeTool(tools.get("aft_zoom")!, { filePath: "src.ts", url: "https://example.com/a.md" }),
    ).rejects.toThrow("not both");
    expect(calls).toHaveLength(0);
  });

  test("aft_zoom targets array uses one tool_call with raw targets", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({
      success: true,
      text: [
        "src/a.ts:1-1 [function foo]",
        "",
        "1: export function foo() {}",
        "",
        "src/b.ts:1-1 [function bar]",
        "",
        "1: export function bar() {}",
      ].join("\n"),
      complete: true,
      targets: [
        {
          targetLabel: "src/a.ts",
          name: "foo",
          response: {
            success: true,
            name: "foo",
            kind: "function",
            range: { start_line: 1, end_line: 1 },
            content: "export function foo() {}",
          },
        },
        {
          targetLabel: "src/b.ts",
          name: "bar",
          response: {
            success: true,
            name: "bar",
            kind: "function",
            range: { start_line: 1, end_line: 1 },
            content: "export function bar() {}",
          },
        },
      ],
    }));
    registerReadingTools(api, makePluginContext(bridge), { outline: false, zoom: true });

    const result = (await executeTool(tools.get("aft_zoom")!, {
      targets: [
        { filePath: "src/a.ts", symbol: "foo" },
        { filePath: "src/b.ts", symbol: "bar" },
      ],
    })) as { content: Array<{ text: string }> };

    expect(calls).toHaveLength(1);
    expect(calls[0].command).toBe("tool_call");
    expect(calls[0].params.name).toBe("zoom");
    expect(toolArgs(calls[0])).toEqual({
      targets: [
        { filePath: "src/a.ts", symbol: "foo" },
        { filePath: "src/b.ts", symbol: "bar" },
      ],
    });
    expect(result.content[0].text).toContain("src/a.ts:1-1 [function foo]");
    expect(result.content[0].text).toContain("src/b.ts:1-1 [function bar]");
  });

  test("aft_zoom targets returns server partial text without throwing", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({
      success: true,
      text: [
        "Incomplete zoom results: one or more symbols failed.",
        "",
        "src/a.ts:1-1 [function ok]",
        "",
        "1: export function ok() {}",
        "",
        'Symbol "missing" not found in src/b.ts: symbol not found',
      ].join("\n"),
      complete: false,
      targets: [
        {
          targetLabel: "src/a.ts",
          name: "ok",
          response: {
            success: true,
            name: "ok",
            kind: "function",
            range: { start_line: 1, end_line: 1 },
            content: "export function ok() {}",
          },
        },
        {
          targetLabel: "src/b.ts",
          name: "missing",
          response: { success: false, message: "symbol not found" },
        },
      ],
    }));
    registerReadingTools(api, makePluginContext(bridge), { outline: false, zoom: true });

    const result = (await executeTool(tools.get("aft_zoom")!, {
      targets: [
        { filePath: "src/a.ts", symbol: "ok" },
        { filePath: "src/b.ts", symbol: "missing" },
      ],
    })) as { content: Array<{ text: string }> };

    expect(calls).toHaveLength(1);
    expect(result.content[0].text).toContain("Incomplete zoom results");
    expect(result.content[0].text).toContain('Symbol "missing" not found in src/b.ts:');
    expect(result.content[0].text).toContain("src/a.ts:1-1 [function ok]");
  });

  test("aft_zoom targets is mutually exclusive with filePath/symbols/url", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge();
    registerReadingTools(api, makePluginContext(bridge), { outline: false, zoom: true });

    await expect(
      executeTool(tools.get("aft_zoom")!, {
        targets: [{ filePath: "src/a.ts", symbol: "foo" }],
        filePath: "src/a.ts",
        symbols: "foo",
      }),
    ).rejects.toThrow(/mutually exclusive/);
    expect(calls).toHaveLength(0);
  });

  test("aft_zoom treats empty-content targets shapes as not provided (GPT empty-param regression)", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "ok" }));
    registerReadingTools(api, makePluginContext(bridge), { outline: false, zoom: true });

    // Array of empty-content entries — GPT models send this shape instead of omitting `targets`.
    // Plus `url: ""` (empty string). Both must be treated as not-provided so filePath wins
    // and we don't get a misleading "targets is mutually exclusive" error.
    await executeTool(tools.get("aft_zoom")!, {
      filePath: "src.ts",
      url: "",
      targets: [{ filePath: "", symbol: "" }],
      symbols: "ok",
    });
    expect(calls).toHaveLength(1);
    expect(toolArgs(calls[0])).toEqual({ filePath: "src.ts", symbols: "ok" });

    // Single object form, same all-empty pattern.
    calls.length = 0;
    await executeTool(tools.get("aft_zoom")!, {
      filePath: "src.ts",
      targets: { filePath: "", symbol: "" },
      symbols: "ok",
    });
    expect(calls).toHaveLength(1);
    expect(toolArgs(calls[0])).toEqual({ filePath: "src.ts", symbols: "ok" });
  });

  test("aft_zoom symbols accepts string form for single-symbol lookup", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({
      success: true,
      text: "src.ts:1-1 [function ok]\n\n1: export function ok() {}",
      name: "ok",
      kind: "function",
      range: { start_line: 1, end_line: 1 },
      content: "export function ok() {}",
    }));
    registerReadingTools(api, makePluginContext(bridge), { outline: false, zoom: true });

    const result = (await executeTool(tools.get("aft_zoom")!, {
      filePath: "src.ts",
      symbols: "ok",
    })) as { content: Array<{ text: string }> };

    expect(calls).toHaveLength(1);
    expect(toolArgs(calls[0])).toEqual({ filePath: "src.ts", symbols: "ok" });
    expect(result.content[0].text).not.toContain("Incomplete zoom results");
    expect(result.content[0].text).toContain("src.ts:1-1 [function ok]");
  });

  test("aft_zoom symbols array is passed through without joining", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({
      success: true,
      text: "Section A\n\nBody A\n\nSection B\n\nBody B",
      complete: true,
      symbols: [
        { name: "Section A", response: { success: true, name: "Section A", content: "Body A" } },
        { name: "Section B", response: { success: true, name: "Section B", content: "Body B" } },
      ],
    }));
    registerReadingTools(api, makePluginContext(bridge), { outline: false, zoom: true });

    const result = (await executeTool(tools.get("aft_zoom")!, {
      url: "https://example.com/doc.md",
      symbols: ["Section A", "Section B"],
    })) as { content: Array<{ text: string }> };

    expect(calls).toHaveLength(1);
    expect(toolArgs(calls[0])).toEqual({
      url: "https://example.com/doc.md",
      symbols: ["Section A", "Section B"],
    });
    expect(result.content[0].text).toContain("Body A");
    expect(result.content[0].text).toContain("Body B");
  });

  test("aft_zoom targets accepts single object form for one-target lookup", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({
      success: true,
      text: "src/a.ts:1-2 [function foo]\n\n1: export function foo() {}",
    }));
    registerReadingTools(api, makePluginContext(bridge), { outline: false, zoom: true });

    const result = (await executeTool(tools.get("aft_zoom")!, {
      targets: { filePath: "src/a.ts", symbol: "foo" },
    })) as { content: Array<{ text: string }> };

    expect(calls).toHaveLength(1);
    expect(toolArgs(calls[0])).toEqual({ targets: [{ filePath: "src/a.ts", symbol: "foo" }] });
    expect(result.content[0].text).toContain("src/a.ts:1-2 [function foo]");
  });

  test("aft_zoom threads callgraph true to all zoom request shapes and omits it by default", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "ok" }));
    registerReadingTools(api, makePluginContext(bridge), { outline: false, zoom: true });

    await executeTool(tools.get("aft_zoom")!, {
      targets: [{ filePath: "src/a.ts", symbol: "foo" }],
      callgraph: true,
    });
    await executeTool(tools.get("aft_zoom")!, {
      filePath: "src/a.ts",
      symbols: ["foo"],
      callgraph: true,
    });
    await executeTool(tools.get("aft_zoom")!, { filePath: "src/a.ts", callgraph: true });

    expect(calls.map((call) => toolArgs(call))).toEqual([
      { targets: [{ filePath: "src/a.ts", symbol: "foo" }], callgraph: true },
      { filePath: "src/a.ts", symbols: ["foo"], callgraph: true },
      { filePath: "src/a.ts", callgraph: true },
    ]);

    calls.length = 0;
    await executeTool(tools.get("aft_zoom")!, { filePath: "src/a.ts", symbols: "foo" });
    expect(toolArgs(calls[0])).toEqual({ filePath: "src/a.ts", symbols: "foo" });
  });

  test("aft_zoom schema does not expose read-style line ranges", () => {
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge();
    registerReadingTools(api, makePluginContext(bridge), { outline: false, zoom: true });

    const schema = JSON.stringify(tools.get("aft_zoom")!.parameters);
    expect(schema).not.toContain("startLine");
    expect(schema).not.toContain("endLine");
  });
});
