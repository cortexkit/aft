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

async function tempProject(): Promise<string> {
  const root = await mkdtemp(join(tmpdir(), "aft-pi-reading-"));
  tempRoots.push(root);
  return root;
}

afterEach(async () => {
  await Promise.all(tempRoots.splice(0).map((root) => rm(root, { recursive: true, force: true })));
});

describe("reading tool adapters", () => {
  test("aft_outline maps a target array to the bridge files request", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "outline" }));
    registerReadingTools(api, makePluginContext(bridge), { outline: true, zoom: true });

    await executeTool(tools.get("aft_outline")!, { target: ["src/a.ts", "src/b.ts"] });

    expect(calls[0].command).toBe("outline");
    expect(calls[0].params).toEqual({ files: ["src/a.ts", "src/b.ts"] });
  });

  test("aft_outline detects directories and sends an absolute directory path", async () => {
    const root = await tempProject();
    await mkdir(join(root, "src"));
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, files: [] }));
    registerReadingTools(api, makePluginContext(bridge), { outline: true, zoom: false });

    await executeTool(tools.get("aft_outline")!, { target: "src" }, { cwd: root } as never);

    expect(calls[0].command).toBe("outline");
    expect(calls[0].params).toEqual({ directory: join(root, "src") });
  });

  test("aft_outline treats missing local paths as file targets and lets Rust report errors", async () => {
    const root = await tempProject();
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "missing" }));
    registerReadingTools(api, makePluginContext(bridge), { outline: true, zoom: false });

    await executeTool(tools.get("aft_outline")!, { target: "src/missing.ts" }, {
      cwd: root,
    } as never);

    expect(calls[0].params).toEqual({ file: "src/missing.ts" });
  });

  test("aft_zoom maps contextLines to each batched symbol request and preserves failures", async () => {
    const root = await tempProject();
    await writeFile(join(root, "src.ts"), "export function ok() {}\n");
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge((_command, params) => {
      if (params.symbol === "missing") return { success: false, message: "not found" };
      return { success: true, symbol: params.symbol, text: "1: export function ok() {}" };
    });
    registerReadingTools(api, makePluginContext(bridge), { outline: true, zoom: true });

    const result = (await executeTool(tools.get("aft_zoom")!, {
      filePath: "src.ts",
      symbols: ["ok", "missing"],
      contextLines: 2,
    })) as { content: Array<{ text: string }> };

    expect(calls.map((call) => call.params)).toEqual([
      { file: "src.ts", symbol: "ok", context_lines: 2 },
      { file: "src.ts", symbol: "missing", context_lines: 2 },
    ]);
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
});
