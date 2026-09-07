/// <reference path="../bun-test.d.ts" />
/**
 * Unit tests for aft_outline/aft_zoom argument shaping.
 */

import { afterEach, describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, realpath, rm } from "node:fs/promises";
import { join } from "node:path";
import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { readingTools } from "../tools/reading.js";
import type { PluginContext } from "../types.js";
import { noopAsk, toolResultText } from "./test-helpers";

type BridgeResponse = Record<string, unknown>;
type SendCall = { command: string; params: Record<string, unknown> };
type ToolCallCall = {
  sessionId: string | undefined;
  name: string;
  rawArgs: Record<string, unknown>;
};
type AskCall = {
  permission?: string;
  patterns?: string[];
  metadata?: Record<string, unknown>;
};

const tempRoots: string[] = [];

async function tempProject(): Promise<string> {
  const root = await realpath(await mkdtemp(join(process.cwd(), ".aft-opencode-reading-")));
  tempRoots.push(root);
  return root;
}

function createMockClient(): any {
  return {
    lsp: {
      status: async () => ({ data: [] }),
    },
    find: {
      symbols: async () => ({ data: [] }),
    },
  };
}

function createPluginContext(
  pool: BridgePool,
  overrides: Partial<PluginContext> = {},
): PluginContext {
  return {
    pool,
    client: createMockClient(),
    config: {} as PluginContext["config"],
    storageDir: "/tmp/aft-reading-test",
    ...overrides,
  };
}

function createMockSdkContext(directory: string, ask: ToolContext["ask"] = noopAsk): ToolContext {
  return {
    sessionID: "reading-session",
    messageID: "message-id",
    agent: "test",
    directory,
    worktree: directory,
    abort: new AbortController().signal,
    metadata: () => {},
    ask,
  };
}

function recordingAsk(calls: AskCall[]): ToolContext["ask"] {
  return (async (input: AskCall) => {
    calls.push(input);
  }) as unknown as ToolContext["ask"];
}

function createMockReadingHarness(
  sendImpl: (
    command: string,
    params: Record<string, unknown>,
  ) => Promise<BridgeResponse> | BridgeResponse,
) {
  const sendCalls: SendCall[] = [];
  const toolCallCalls: ToolCallCall[] = [];
  const bridge = {
    send: async (command: string, params: Record<string, unknown>) => {
      sendCalls.push({ command, params });
      return await sendImpl(command, params);
    },
    toolCall: async (
      sessionId: string | undefined,
      name: string,
      rawArgs: Record<string, unknown> = {},
    ) => {
      toolCallCalls.push({ sessionId, name, rawArgs });
      return await sendImpl(name, rawArgs);
    },
  };
  const pool = {
    getBridge: () => bridge,
  } as unknown as BridgePool;

  return {
    sendCalls,
    toolCallCalls,
    tools: readingTools(createPluginContext(pool)),
  };
}

afterEach(async () => {
  await Promise.all(tempRoots.splice(0).map((root) => rm(root, { recursive: true, force: true })));
});

describe("reading tool adapters", () => {
  test("describes outline guidance without invalid read callgraph syntax", () => {
    const pool = {} as BridgePool;
    const enabled = readingTools(createPluginContext(pool)).aft_outline.description;
    const disabled = readingTools(
      createPluginContext(pool, { config: { disabled_tools: ["aft_zoom"] } as never }),
    ).aft_outline.description;
    expect(enabled).toContain("aft_zoom with `callgraph:true`");
    expect(enabled).toContain("breadth-first with directory rollups");
    expect(enabled).toContain("line count");
    expect(enabled).not.toContain("up to 200 files");
    expect(enabled).not.toContain("byte metadata");
    expect(disabled).toContain("prefer aft_search + read on named symbols");
    expect(disabled).not.toContain("aft_zoom");
    expect(disabled).not.toContain("read with `callgraph:true`");
  });
  test("aft_outline files:true forwards raw target through tool_call and returns server text", async () => {
    const root = await tempProject();
    await mkdir(join(root, "src"));
    const serverText = [
      "path | language | symbols",
      "",
      "⚠ Partial result: walk truncated at 200 files. 12 additional files in this directory were not indexed.",
      "Unchecked files:",
      "  src/overflow-1.ts",
    ].join("\n");
    const { sendCalls, toolCallCalls, tools } = createMockReadingHarness(() => ({
      success: true,
      text: serverText,
      complete: false,
      walk_truncated: true,
      unchecked_files: ["src/overflow-1.ts"],
    }));

    const output = await tools.aft_outline.execute(
      { target: "src", files: true },
      createMockSdkContext(root),
    );

    expect(sendCalls).toEqual([]);
    expect(toolCallCalls).toEqual([
      {
        sessionId: "reading-session",
        name: "outline",
        rawArgs: { target: "src", files: true },
      },
    ]);
    expect(output).toBe(serverText);
  });

  test("aft_outline files:true asks external_directory for an out-of-project directory", async () => {
    const tmpRoot = await tempProject();
    const project = join(tmpRoot, "project");
    const external = join(tmpRoot, "external");
    await mkdir(project, { recursive: true });
    await mkdir(external, { recursive: true });
    const askCalls: AskCall[] = [];
    const { sendCalls, toolCallCalls, tools } = createMockReadingHarness(() => ({
      success: true,
      text: "external files",
    }));

    await tools.aft_outline.execute(
      { target: external, files: true },
      createMockSdkContext(project, recordingAsk(askCalls)),
    );

    const externalAsks = askCalls.filter((call) => call.permission === "external_directory");
    expect(externalAsks).toHaveLength(1);
    expect(externalAsks[0]?.patterns).toEqual([join(external, "*").replaceAll("\\", "/")]);
    expect(externalAsks[0]?.metadata?.filepath).toBe(external);
    expect(sendCalls).toEqual([]);
    expect(toolCallCalls[0]).toMatchObject({
      sessionId: "reading-session",
      name: "outline",
      rawArgs: { target: external, files: true },
    });
  });

  test("aft_outline files:true target arrays ask once per unique external target", async () => {
    const tmpRoot = await tempProject();
    const project = join(tmpRoot, "project");
    const externalRoot = join(tmpRoot, "external");
    const first = join(externalRoot, "first");
    const second = join(externalRoot, "second");
    await mkdir(project, { recursive: true });
    await mkdir(first, { recursive: true });
    await mkdir(second, { recursive: true });
    const askCalls: AskCall[] = [];
    const { sendCalls, toolCallCalls, tools } = createMockReadingHarness(() => ({
      success: true,
      text: "external files",
    }));

    await tools.aft_outline.execute(
      { target: [first, second], files: true },
      createMockSdkContext(project, recordingAsk(askCalls)),
    );

    const externalAsks = askCalls.filter((call) => call.permission === "external_directory");
    expect(externalAsks).toHaveLength(2);
    expect(externalAsks.map((call) => call.patterns?.[0])).toEqual([
      join(first, "*").replaceAll("\\", "/"),
      join(second, "*").replaceAll("\\", "/"),
    ]);
    expect(sendCalls).toEqual([]);
    expect(toolCallCalls[0]).toMatchObject({
      sessionId: "reading-session",
      name: "outline",
      rawArgs: { target: [first, second], files: true },
    });
  });

  test("aft_zoom targets array forwards one native-leaf tool_call across different files", async () => {
    const root = await tempProject();
    const { sendCalls, toolCallCalls, tools } = createMockReadingHarness(() => ({
      success: true,
      text: [
        "src/a.ts:10-20 [function foo]",
        "",
        "10: // body of foo",
        "",
        "src/b.ts:10-20 [function bar]",
        "",
        "10: // body of bar",
      ].join("\n"),
    }));

    const result = toolResultText(
      await tools.aft_zoom.execute(
        {
          targets: [
            { filePath: "src/a.ts", symbol: "foo" },
            { filePath: "src/b.ts", symbol: "bar" },
          ],
        },
        createMockSdkContext(root),
      ),
    );

    expect(sendCalls).toHaveLength(0);
    expect(toolCallCalls).toEqual([
      {
        sessionId: "reading-session",
        name: "zoom",
        rawArgs: {
          targets: [
            { filePath: "src/a.ts", symbol: "foo" },
            { filePath: "src/b.ts", symbol: "bar" },
          ],
        },
      },
    ]);
    // Each section uses its OWN filePath as the header label, not a shared one.
    expect(result).toContain("src/a.ts:10-20 [function foo]");
    expect(result).toContain("src/b.ts:10-20 [function bar]");
  });

  test("aft_zoom targets returns server-rendered per-entry failure text", async () => {
    const root = await tempProject();
    const { toolCallCalls, tools } = createMockReadingHarness(() => ({
      success: true,
      text: [
        "Incomplete zoom results: one or more symbols failed.",
        "",
        "src/a.ts:1-2 [function ok]",
        "",
        "1: ok",
        "",
        `Symbol "missing" not found in src/b.ts: symbol 'missing' not found`,
      ].join("\n"),
    }));

    const text = toolResultText(
      await tools.aft_zoom.execute(
        {
          targets: [
            { filePath: "src/a.ts", symbol: "ok" },
            { filePath: "src/b.ts", symbol: "missing" },
          ],
        },
        createMockSdkContext(root),
      ),
    );

    expect(toolCallCalls).toHaveLength(1);
    expect(text).toContain("Incomplete zoom results");
    expect(text).toContain('Symbol "missing" not found in src/b.ts:');
    expect(text).toContain("src/a.ts:1-2 [function ok]");
  });

  test("aft_zoom targets is mutually exclusive with filePath/symbols/url", async () => {
    const root = await tempProject();
    const { sendCalls, toolCallCalls, tools } = createMockReadingHarness(() => ({ success: true }));

    await expect(
      tools.aft_zoom.execute(
        {
          targets: [{ filePath: "src/a.ts", symbol: "foo" }],
          filePath: "src/a.ts",
          symbols: "foo",
        },
        createMockSdkContext(root),
      ),
    ).rejects.toThrow(/mutually exclusive/);
    expect(sendCalls).toHaveLength(0);
    expect(toolCallCalls).toHaveLength(0);
  });

  test("aft_zoom treats empty-content targets shapes as not provided (GPT empty-param regression)", async () => {
    const root = await tempProject();
    await Bun.write(`${root}/src/a.ts`, "export function foo() {}\nexport function bar() {}\n");
    const { sendCalls, toolCallCalls, tools } = createMockReadingHarness(() => ({
      success: true,
      file: "src/a.ts",
      symbol: "foo",
      text: "export function foo() {}",
    }));

    // Array of empty-content entries — GPT models send this shape instead of omitting `targets`.
    // Plus `url: ""` (empty string). Both must be treated as not-provided so filePath wins
    // and we don't get a misleading "targets is mutually exclusive" error.
    await tools.aft_zoom.execute(
      {
        filePath: "src/a.ts",
        url: "",
        targets: [{ filePath: "", symbol: "" }],
        symbols: "foo",
      },
      createMockSdkContext(root),
    );
    expect(sendCalls).toHaveLength(0);
    expect(toolCallCalls).toEqual([
      {
        sessionId: "reading-session",
        name: "zoom",
        rawArgs: { filePath: "src/a.ts", symbols: "foo" },
      },
    ]);

    // Single object form, same all-empty pattern.
    toolCallCalls.length = 0;
    await tools.aft_zoom.execute(
      {
        filePath: "src/a.ts",
        targets: { filePath: "", symbol: "" },
        symbols: "foo",
      },
      createMockSdkContext(root),
    );
    expect(sendCalls).toHaveLength(0);
    expect(toolCallCalls).toEqual([
      {
        sessionId: "reading-session",
        name: "zoom",
        rawArgs: { filePath: "src/a.ts", symbols: "foo" },
      },
    ]);
  });

  test("aft_zoom string symbols forwards one tool_call and returns server-rendered batch text", async () => {
    const root = await tempProject();
    const { sendCalls, toolCallCalls, tools } = createMockReadingHarness(() => ({
      success: true,
      text: "server-rendered batch text",
      complete: true,
      symbols: [
        {
          name: "a",
          response: {
            success: true,
            name: "a",
            kind: "function",
            range: { start_line: 1, end_line: 2 },
            content: "function a() {}\n",
            context_before: [],
            context_after: [],
            annotations: { calls_out: [], called_by: [] },
          },
        },
        {
          name: "b",
          response: {
            success: true,
            name: "b",
            kind: "function",
            range: { start_line: 5, end_line: 6 },
            content: "function b() {}\n",
            context_before: [],
            context_after: [],
            annotations: { calls_out: [], called_by: [] },
          },
        },
      ],
    }));

    const text = toolResultText(
      await tools.aft_zoom.execute(
        { filePath: "src/job.rs", symbols: "a b" },
        createMockSdkContext(root),
      ),
    );

    expect(sendCalls).toHaveLength(0);
    expect(toolCallCalls).toEqual([
      {
        sessionId: "reading-session",
        name: "zoom",
        rawArgs: { filePath: "src/job.rs", symbols: "a b" },
      },
    ]);
    expect(text).toBe("server-rendered batch text");
  });

  test("aft_zoom string symbols returns server-rendered incomplete framing", async () => {
    const root = await tempProject();
    const { tools } = createMockReadingHarness(() => ({
      success: true,
      text: 'Incomplete zoom results\n\nfunction a() {}\n\nSymbol "missing" not found:',
      complete: false,
      symbols: [
        {
          name: "a",
          response: {
            success: true,
            name: "a",
            kind: "function",
            range: { start_line: 1, end_line: 1 },
            content: "function a() {}\n",
            context_before: [],
            context_after: [],
            annotations: { calls_out: [], called_by: [] },
          },
        },
        {
          name: "missing",
          response: {
            success: false,
            code: "symbol_not_found",
            message: "symbol 'missing' not found",
          },
        },
      ],
    }));

    const text = toolResultText(
      await tools.aft_zoom.execute(
        { filePath: "src/job.rs", symbols: "a missing" },
        createMockSdkContext(root),
      ),
    );

    expect(text).toContain("Incomplete zoom results");
    expect(text).toContain("function a() {}");
    expect(text).toContain('Symbol "missing" not found:');
  });

  test("aft_zoom targets rejects empty filePath/symbol entries", async () => {
    const root = await tempProject();
    const { sendCalls, toolCallCalls, tools } = createMockReadingHarness(() => ({ success: true }));

    await expect(
      tools.aft_zoom.execute(
        { targets: [{ filePath: "src/a.ts", symbol: "" }] },
        createMockSdkContext(root),
      ),
    ).rejects.toThrow(/targets\[0\]\.symbol/);

    // An empty sentinel on a required path field means the field was not
    // supplied in substance, so the error names the canonical `path` as
    // missing (v0.49 surface rule) rather than the legacy `filePath` alias.
    await expect(
      tools.aft_zoom.execute(
        { targets: [{ filePath: "", symbol: "x" }] },
        createMockSdkContext(root),
      ),
    ).rejects.toThrow(/targets\[0\]\.path/);

    expect(sendCalls).toHaveLength(0);
    expect(toolCallCalls).toHaveLength(0);
  });

  test("aft_zoom threads callgraph true to all zoom request shapes and omits it by default", async () => {
    const root = await tempProject();
    const { sendCalls, toolCallCalls, tools } = createMockReadingHarness((_command, params) => ({
      success: true,
      text: "ok",
      name: (params.symbol as string | undefined) ?? "lines",
      kind: params.symbol ? "function" : "lines",
      range: { start_line: 1, end_line: 1 },
      content: "ok\n",
    }));

    await tools.aft_zoom.execute(
      { targets: [{ filePath: "src/a.ts", symbol: "foo" }], callgraph: true },
      createMockSdkContext(root),
    );
    await tools.aft_zoom.execute(
      { filePath: "src/a.ts", symbols: ["foo"], callgraph: true },
      createMockSdkContext(root),
    );
    await tools.aft_zoom.execute(
      { filePath: "src/a.ts", callgraph: true },
      createMockSdkContext(root),
    );

    expect(sendCalls).toHaveLength(0);
    expect(toolCallCalls).toEqual([
      {
        sessionId: "reading-session",
        name: "zoom",
        rawArgs: { targets: [{ filePath: "src/a.ts", symbol: "foo" }], callgraph: true },
      },
      {
        sessionId: "reading-session",
        name: "zoom",
        rawArgs: { filePath: "src/a.ts", symbols: ["foo"], callgraph: true },
      },
      {
        sessionId: "reading-session",
        name: "zoom",
        rawArgs: { filePath: "src/a.ts", callgraph: true },
      },
    ]);

    sendCalls.length = 0;
    toolCallCalls.length = 0;
    await tools.aft_zoom.execute(
      { filePath: "src/a.ts", symbols: "foo" },
      createMockSdkContext(root),
    );
    expect(sendCalls).toHaveLength(0);
    expect(toolCallCalls[0]).toEqual({
      sessionId: "reading-session",
      name: "zoom",
      rawArgs: { filePath: "src/a.ts", symbols: "foo" },
    });
    expect(toolCallCalls[0]?.rawArgs).not.toHaveProperty("callgraph");
  });
});
