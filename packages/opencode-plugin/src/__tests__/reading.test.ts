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
          batch: [
            { target: "src/a.ts", symbols: "foo" },
            { target: "src/b.ts", symbols: "bar" },
          ],
        },
      },
    ]);
    // Each section uses its OWN filePath as the header label, not a shared one.
    expect(result).toContain("src/a.ts:10-20 [function foo]");
    expect(result).toContain("src/b.ts:10-20 [function bar]");
  });

  test("aft_zoom parses JSON-stringified targets before dispatch", async () => {
    const root = await tempProject();
    const { toolCallCalls, tools } = createMockReadingHarness(() => ({
      success: true,
      text: "ok",
    }));
    for (const targets of [
      [{ path: "src/a.ts", symbol: "foo" }],
      [
        { path: "src/a.ts", symbol: "foo" },
        { path: "src/b.ts", symbol: "bar" },
      ],
    ]) {
      await tools.aft_zoom.execute(
        { targets: JSON.stringify(targets) },
        createMockSdkContext(root),
      );
      expect(toolCallCalls.at(-1)?.rawArgs).toEqual({
        batch: targets.map(({ path, symbol }) => ({ target: path, symbols: symbol })),
      });
    }
    await tools.aft_zoom.execute(
      { targets: JSON.stringify({ path: "src/a.ts", symbol: "foo" }) },
      createMockSdkContext(root),
    );
    expect(toolCallCalls.at(-1)?.rawArgs).toEqual({
      batch: [{ target: "src/a.ts", symbols: "foo" }],
    });
    expect(toolCallCalls).toHaveLength(3);
  });

  test("aft_zoom rejects malformed or wrong-type targets with a targets-specific error", async () => {
    const root = await tempProject();
    const { toolCallCalls, tools } = createMockReadingHarness(() => ({ success: true }));
    for (const targets of ["[{bad", "not JSON", "42", 42]) {
      await expect(tools.aft_zoom.execute({ targets }, createMockSdkContext(root))).rejects.toThrow(
        /targets must be an array of \{target, symbols\}/,
      );
    }
    expect(toolCallCalls).toHaveLength(0);
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

  test("aft_zoom merges targets with path + symbols into one batch (placeholder-filling models)", async () => {
    // Models that fill every declared property send a real path + symbols
    // request next to a placeholder target such as {path: "x", symbol: "x"}.
    // Refusing the pair lost the real request; both now go to the server as
    // one batch, and the unresolvable target reports its own error line.
    const root = await tempProject();
    const { sendCalls, toolCallCalls, tools } = createMockReadingHarness(() => ({
      success: true,
      text: "ok",
    }));

    await tools.aft_zoom.execute(
      {
        targets: { path: "x", symbol: "x" },
        filePath: "src/a.ts",
        symbols: ["foo", "bar"],
        url: "",
        contextLines: 0,
        callgraph: false,
      },
      createMockSdkContext(root),
    );
    expect(sendCalls).toHaveLength(0);
    expect(toolCallCalls.map((call) => call.rawArgs)).toEqual([
      {
        batch: [
          { target: "src/a.ts", symbols: ["foo", "bar"] },
          { target: "x", symbols: "x" },
        ],
      },
    ]);

    // Different location aliases are refused.
    await expect(
      tools.aft_zoom.execute(
        {
          targets: [{ filePath: "src/a.ts", symbol: "foo" }],
          filePath: "src/a.ts",
          url: "https://example.com/doc.md",
        },
        createMockSdkContext(root),
      ),
    ).rejects.toThrow(/Conflicting/);
    expect(toolCallCalls).toHaveLength(1);
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
        rawArgs: { batch: [{ target: "src/a.ts", symbols: "foo" }] },
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
        rawArgs: { batch: [{ target: "src/a.ts", symbols: "foo" }] },
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
        rawArgs: { batch: [{ target: "src/job.rs", symbols: "a b" }] },
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

  test("aft_zoom forwards partial targets entries for the server's per-target errors", async () => {
    const root = await tempProject();
    const { sendCalls, toolCallCalls, tools } = createMockReadingHarness(() => ({
      success: true,
      text: "ok",
    }));

    // An entry missing its symbol or its path is no longer refused here: the
    // server answers the rest of the batch and reports that entry on its own
    // line (or refuses the call when no entry is usable).
    await tools.aft_zoom.execute(
      {
        targets: [
          { filePath: "src/a.ts", symbol: "" },
          { filePath: "", symbol: "x" },
        ],
      },
      createMockSdkContext(root),
    );

    expect(sendCalls).toHaveLength(0);
    expect(toolCallCalls.map((call) => call.rawArgs)).toEqual([
      {
        batch: [
          { target: "src/a.ts", symbols: "" },
          { target: "", symbols: "x" },
        ],
      },
    ]);
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
        rawArgs: { batch: [{ target: "src/a.ts", symbols: "foo" }], callgraph: true },
      },
      {
        sessionId: "reading-session",
        name: "zoom",
        rawArgs: { batch: [{ target: "src/a.ts", symbols: ["foo"] }], callgraph: true },
      },
      {
        sessionId: "reading-session",
        name: "zoom",
        rawArgs: { batch: [{ target: "src/a.ts", symbols: "" }], callgraph: true },
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
      rawArgs: { batch: [{ target: "src/a.ts", symbols: "foo" }] },
    });
    expect(toolCallCalls[0]?.rawArgs).not.toHaveProperty("callgraph");
  });
});

test("zoom advertises only a required non-empty batch and shared options", () => {
  const harness = createMockReadingHarness(() => ({ success: true, text: "zoom" }));
  const definition = harness.tools.aft_zoom;
  expect(Object.keys(definition.args).sort()).toEqual(["batch", "callgraph", "contextLines"]);
  expect(definition.args.batch.safeParse([]).success).toBe(false);
  expect(definition.args.batch.safeParse([{ target: "a.ts", symbols: "main" }]).success).toBe(true);
  expect(definition.description).toContain("batch: [{ target, symbols }]");
  expect(definition.description).not.toContain("{ path, symbols }");
  expect(definition.description).not.toContain("{ url, symbols }");
});

test("zoom canonical batch mixes files and URLs and preserves symbol arrays", async () => {
  const harness = createMockReadingHarness(() => ({ success: true, text: "zoom" }));
  const batch = [
    { target: "http_client.rs", symbols: ["client", "connect"] },
    { target: "https://example.test/docs", symbols: "Overview" },
  ];
  await harness.tools.aft_zoom.execute(
    { batch, contextLines: 2, callgraph: true },
    createMockSdkContext(process.cwd()),
  );
  expect(harness.toolCallCalls[0].rawArgs).toEqual({ batch, contextLines: 2, callgraph: true });
});

test("zoom quietly maps every legacy location shape to batch", async () => {
  for (const key of ["target", "path", "filePath", "url"]) {
    for (const input of [
      { [key]: "https://example.test/docs", symbols: "Overview" },
      { targets: [{ [key]: "https://example.test/docs", symbol: "Overview" }] },
    ]) {
      const harness = createMockReadingHarness(() => ({ success: true, text: "zoom" }));
      await harness.tools.aft_zoom.execute(input, createMockSdkContext(process.cwd()));
      expect(harness.toolCallCalls[0].rawArgs).toEqual({
        batch: [{ target: "https://example.test/docs", symbols: "Overview" }],
      });
    }
  }
});

test("zoom refuses empty batch and conflicting locations before dispatch", async () => {
  const harness = createMockReadingHarness(() => ({ success: true, text: "zoom" }));
  const tool = harness.tools.aft_zoom;
  await expect(tool.execute({ batch: [] }, createMockSdkContext(process.cwd()))).rejects.toThrow(
    "'batch'",
  );
  await expect(
    tool.execute(
      { batch: [{ target: "a.rs", url: "b.rs", symbols: "main" }] },
      createMockSdkContext(process.cwd()),
    ),
  ).rejects.toThrow("'b.rs'");
  expect(harness.toolCallCalls).toHaveLength(0);
});

test("committed zoom schema pins the batch-only surface without root unions", async () => {
  const schemas = await Bun.file(
    new URL("../../../../crates/aft/src/subc_tool_schemas.json", import.meta.url),
  ).json();
  const schema = schemas.zoom;
  expect(Object.keys(schema.properties).sort()).toEqual(["batch", "callgraph", "contextLines"]);
  expect(schema.required).toEqual(["batch"]);
  expect(schema.properties.batch.minItems).toBe(1);
  expect(Object.keys(schema.properties.batch.items.properties).sort()).toEqual([
    "symbols",
    "target",
  ]);
  expect(schema.properties.batch.items.required.sort()).toEqual(["symbols", "target"]);
  for (const key of ["anyOf", "oneOf", "allOf"]) expect(schema[key]).toBeUndefined();
});
