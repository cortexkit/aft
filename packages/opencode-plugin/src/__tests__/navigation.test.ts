/// <reference path="../bun-test.d.ts" />

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { BinaryBridge } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";

import { navigationTools } from "../tools/navigation.js";
import type { PluginContext } from "../types.js";
import { noopAsk } from "./test-helpers";

let projectRoot: string;

beforeAll(() => {
  projectRoot = mkdtempSync(join(tmpdir(), "aft-test-repo-"));
});

afterAll(() => {
  rmSync(projectRoot, { recursive: true, force: true });
});

type BridgeResponse = Record<string, unknown>;
type SendCall = { command: string; params: Record<string, unknown> };

function makeMockBridge(
  handler: (
    command: string,
    params: Record<string, unknown>,
  ) => Promise<BridgeResponse> | BridgeResponse = () => ({ success: true }),
): { bridge: BinaryBridge; calls: SendCall[] } {
  const calls: SendCall[] = [];
  const bridge = {
    async send(command: string, params: Record<string, unknown>) {
      calls.push({ command, params });
      return handler(command, params);
    },
    async toolCall(_sessionId: string | undefined, name: string, rawArgs: Record<string, unknown>) {
      calls.push({ command: name, params: rawArgs });
      return handler(name, rawArgs);
    },
  } as unknown as BinaryBridge;
  return { bridge, calls };
}

function makePluginContext(
  bridge: BinaryBridge,
  overrides: Partial<PluginContext> = {},
): PluginContext {
  return {
    pool: { getBridge: () => bridge } as unknown as PluginContext["pool"],
    client: {
      lsp: { status: async () => ({ data: [] }) },
      find: { symbols: async () => ({ data: [] }) },
    } as unknown as PluginContext["client"],
    config: {} as PluginContext["config"],
    storageDir: "/tmp/aft-opencode-tests",
    ...overrides,
  };
}

function makeToolContext(): ToolContext {
  return {
    messageID: "message-id",
    agent: "test",
    directory: projectRoot,
    worktree: projectRoot,
    abort: new AbortController().signal,
    metadata: () => {},
    ask: noopAsk,
  } as unknown as ToolContext;
}

async function expectRejectMessage(action: () => Promise<unknown>): Promise<string> {
  try {
    await action();
  } catch (error) {
    expect(error).toBeInstanceOf(Error);
    return (error as Error).message;
  }
  throw new Error("expected action to reject");
}

describe("aft_callgraph OpenCode adapter", () => {
  test("describes zoom-aware callgraph guidance without invalid read callgraph syntax", () => {
    const bridge = makeMockBridge().bridge;
    const enabled = navigationTools(makePluginContext(bridge)).aft_callgraph.description;
    const disabled = navigationTools(
      makePluginContext(bridge, { config: { disabled_tools: ["aft_zoom"] } as never }),
    ).aft_callgraph.description;
    expect(enabled).toContain("Use aft_zoom with `callgraph:true`");
    expect(disabled).toContain("Use aft_callgraph call_tree (depth 1)");
    expect(disabled).not.toContain("aft_zoom");
    expect(disabled).not.toContain("read with `callgraph:true`");
  });
  test("success path dispatches to the selected op and maps filePath to file", async () => {
    const { bridge, calls } = makeMockBridge((command, params) => ({
      success: true,
      text: "1 affected call site · 1 file",
      command,
      params,
    }));
    const tools = navigationTools(makePluginContext(bridge));

    const raw = await tools.aft_callgraph.execute(
      {
        op: "impact",
        filePath: "src/app.ts",
        symbol: "run",
        depth: 4,
      },
      makeToolContext(),
    );

    expect(typeof raw).toBe("string");
    expect(raw as string).toContain("affected call site");
    expect(raw as string).not.toContain('"success"');
    expect(calls).toHaveLength(1);
    expect(calls[0]).toEqual({
      command: "callgraph",
      params: {
        op: "impact",
        filePath: "src/app.ts",
        symbol: "run",
        depth: 4,
      },
    });
  });

  test("includeUnresolved is exposed and controls call_tree formatting only", async () => {
    const { bridge, calls } = makeMockBridge((_command, params) => ({
      success: true,
      text:
        params.includeUnresolved === true
          ? "len [/repo/src/app.ts:2] [unresolved]\nSome [/repo/src/app.ts:3] [unresolved]"
          : "+ 2 unresolved external calls: len, Some\nproject [/repo/src/project.ts:4]",
    }));
    const tools = navigationTools(makePluginContext(bridge));

    expect(Object.hasOwn(tools.aft_callgraph.args, "includeUnresolved")).toBe(true);
    expect(tools.aft_callgraph.description).toContain("includeUnresolved=true");

    const collapsed = (await tools.aft_callgraph.execute(
      { op: "call_tree", filePath: "src/app.ts", symbol: "run" },
      makeToolContext(),
    )) as string;
    const expanded = (await tools.aft_callgraph.execute(
      { op: "call_tree", filePath: "src/app.ts", symbol: "run", includeUnresolved: true },
      makeToolContext(),
    )) as string;

    expect(collapsed).toContain("+ 2 unresolved external calls: len, Some");
    expect(collapsed).toContain("project [/repo/src/project.ts:4]");
    expect(collapsed).not.toContain("len [/repo/src/app.ts:2] [unresolved]");
    expect(expanded).toContain("len [/repo/src/app.ts:2] [unresolved]");
    expect(expanded).toContain("Some [/repo/src/app.ts:3] [unresolved]");
    expect(expanded).not.toContain("unresolved external calls");
    expect(calls).toHaveLength(2);
    expect(calls[0].params).not.toHaveProperty("includeUnresolved");
    expect(calls[1].params).toHaveProperty("includeUnresolved", true);
  });

  test("trace_to_symbol ambiguous_target errors include candidates (Rust top-level shape)", async () => {
    // Rust's error_with_data() merges extras into the top-level response,
    // so production traffic has `candidates` next to `code`/`message`, NOT
    // nested under `data`.
    const { bridge } = makeMockBridge(() => ({
      success: false,
      code: "ambiguous_target",
      message: 'multiple symbols named "target"',
      text: 'trace_to_symbol: ambiguous_target — multiple symbols named "target". Pass toFile to disambiguate:\n  - file1.rs:42\n  - file2.rs:78',
      candidates: [
        { file: "file1.rs", line: 42, symbol: "target" },
        { file: "file2.rs", line: 78, symbol: "target" },
      ],
    }));
    const tools = navigationTools(makePluginContext(bridge));

    const message = await expectRejectMessage(() =>
      tools.aft_callgraph.execute(
        {
          op: "trace_to_symbol",
          filePath: "src/app.ts",
          symbol: "run",
          toSymbol: "target",
        },
        makeToolContext(),
      ),
    );

    expect(message).toBe(
      'trace_to_symbol: ambiguous_target — multiple symbols named "target". Pass toFile to disambiguate:\n  - file1.rs:42\n  - file2.rs:78',
    );
  });

  test("trace_to_symbol ambiguous_target also works with nested data.candidates (forward compat)", async () => {
    // Keep parsing nested data.* shape too in case any future handler uses it.
    const { bridge } = makeMockBridge(() => ({
      success: false,
      code: "ambiguous_target",
      message: 'multiple symbols named "target"',
      text: 'trace_to_symbol: ambiguous_target — multiple symbols named "target". Pass toFile to disambiguate:\n  - file1.rs:42',
      data: {
        candidates: [{ file: "file1.rs", line: 42 }],
      },
    }));
    const tools = navigationTools(makePluginContext(bridge));

    const message = await expectRejectMessage(() =>
      tools.aft_callgraph.execute(
        {
          op: "trace_to_symbol",
          filePath: "src/app.ts",
          symbol: "run",
          toSymbol: "target",
        },
        makeToolContext(),
      ),
    );

    expect(message).toBe(
      'trace_to_symbol: ambiguous_target — multiple symbols named "target". Pass toFile to disambiguate:\n  - file1.rs:42',
    );
  });

  test("trace_to_symbol target_symbol_not_in_file lists alternate files", async () => {
    const { bridge } = makeMockBridge(() => ({
      success: false,
      code: "target_symbol_not_in_file",
      message: "trace_to_symbol: target symbol 'foo' is not defined in toFile: wrong.rs",
      text: 'trace_to_symbol: target_symbol_not_in_file — multiple symbols named "foo". Try one of these files for toFile:\n  - right1.rs:12\n  - right2.rs:99',
      candidates: [
        { file: "right1.rs", line: 12 },
        { file: "right2.rs", line: 99 },
      ],
    }));
    const tools = navigationTools(makePluginContext(bridge));

    const message = await expectRejectMessage(() =>
      tools.aft_callgraph.execute(
        {
          op: "trace_to_symbol",
          filePath: "src/app.ts",
          symbol: "run",
          toSymbol: "foo",
          toFile: "wrong.rs",
        },
        makeToolContext(),
      ),
    );

    expect(message).toContain("target_symbol_not_in_file");
    expect(message).toContain("Try one of these files for toFile");
    expect(message).toContain("right1.rs:12");
    expect(message).toContain("right2.rs:99");
  });

  test("generic bridge errors keep code, message, and structured extras visible", async () => {
    // A genuine error code (not a soft negative) still throws so it renders as
    // an error, with code/message plus any top-level structured extras visible.
    const { bridge } = makeMockBridge(() => ({
      success: false,
      code: "internal",
      message: "resolver crashed",
      text: 'callers: internal — resolver crashed\ndata: {\n  "file": "src/app.ts",\n  "symbol": "run"\n}',
      file: "src/app.ts",
      symbol: "run",
    }));
    const tools = navigationTools(makePluginContext(bridge));

    const message = await expectRejectMessage(() =>
      tools.aft_callgraph.execute(
        {
          op: "callers",
          filePath: "src/app.ts",
          symbol: "run",
        },
        makeToolContext(),
      ),
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
      const { bridge } = makeMockBridge(() => ({
        success: false,
        code,
        message: `${code} happened`,
        text: `callers: ${code} — ${code} happened`,
        file: "src/app.ts",
        symbol: "run",
      }));
      const tools = navigationTools(makePluginContext(bridge));

      const result = await tools.aft_callgraph.execute(
        { op: "callers", filePath: "src/app.ts", symbol: "run" },
        makeToolContext(),
      );

      expect(typeof result).toBe("string");
      expect(result as string).toContain(`callers: ${code} — ${code} happened`);
    }
  });
});
