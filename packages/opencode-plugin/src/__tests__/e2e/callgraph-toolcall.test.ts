/// <reference path="../../bun-test.d.ts" />

import { mkdir, writeFile } from "node:fs/promises";
import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { navigationTools } from "../../tools/navigation.js";
import type { PluginContext } from "../../types.js";
import { noopAsk } from "../test-helpers";
import {
  cleanupHarnesses,
  createHarness,
  type E2EHarness,
  type HarnessFactory,
  type PreparedBinary,
  prepareBinary,
} from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = describe.skipIf(!initialBinary.binaryPath);

function createPluginContext(harness: E2EHarness): PluginContext {
  return {
    pool: { getBridge: () => harness.bridge } as unknown as BridgePool,
    client: { lsp: {}, find: {} } as PluginContext["client"],
    config: {} as PluginContext["config"],
    storageDir: harness.path(".aft-test-storage"),
  };
}

function createToolContext(harness: E2EHarness): ToolContext {
  return {
    sessionID: "callgraph-toolcall-e2e",
    messageID: "callgraph-toolcall-message",
    agent: "test",
    directory: harness.tempDir,
    worktree: harness.tempDir,
    abort: new AbortController().signal,
    metadata: () => {},
    ask: noopAsk,
  } as ToolContext;
}

const delay = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

export function runCallgraphToolcallSuite(
  options: { harnessFactory?: HarnessFactory; name?: string } = {},
): void {
  maybeDescribe(options.name ?? "e2e aft_callgraph tool_call cutover", () => {
    let preparedBinary: PreparedBinary = initialBinary;
    const harnesses: E2EHarness[] = [];

    beforeAll(async () => {
      preparedBinary = await prepareBinary();
    });

    afterEach(async () => {
      await cleanupHarnesses(harnesses);
    });

    async function harness(): Promise<E2EHarness> {
      const created = await (options.harnessFactory ?? createHarness)(preparedBinary, {
        fixtureNames: ["sample.ts"],
        timeoutMs: 20_000,
        tempPrefix: "aft-plugin-callgraph-toolcall-",
      });
      harnesses.push(created);
      return created;
    }

    async function runCallgraph(
      h: E2EHarness,
      args: Record<string, unknown>,
      options: { allowBuilding?: boolean } = {},
    ): Promise<string> {
      const tools = navigationTools(createPluginContext(h));
      for (let attempt = 0; attempt < 20; attempt++) {
        const output = (await tools.aft_callgraph.execute(args, createToolContext(h))) as string;
        if (options.allowBuilding || !output.includes("callgraph_building")) return output;
        await delay(250);
      }
      throw new Error("callgraph store did not become ready for the e2e fixture");
    }

    test("callers returns server-rendered hits through tool_call", async () => {
      const h = await harness();

      const output = await runCallgraph(h, {
        op: "callers",
        filePath: "sample.ts",
        symbol: "normalize",
      });

      expect(output).toContain("caller");
      expect(output).toContain("funcB");
    });

    if (!options.harnessFactory) {
      test("standalone enables callgraph by default and honors the project config", async () => {
        const args = { op: "callers", filePath: "sample.ts", symbol: "normalize" };
        expect(await runCallgraph(await harness(), args)).toContain("funcB");

        const project = await harness();
        await mkdir(project.path(".cortexkit"), { recursive: true });
        await writeFile(
          project.path(".cortexkit", "aft.jsonc"),
          JSON.stringify({ indexes: { callgraph: false } }),
        );
        const response = await project.bridge.toolCall(
          "standalone-index-settings",
          "callgraph",
          args,
        );
        expect(response.code).toBe("callgraph_off");
      });
    }

    test("call_tree returns forward calls through tool_call", async () => {
      const h = await harness();

      const output = await runCallgraph(h, {
        op: "call_tree",
        filePath: "sample.ts",
        symbol: "funcB",
      });

      expect(output).toContain("funcB");
      expect(output).toContain("normalize");
    });

    test("trace_to_symbol returns a path through tool_call", async () => {
      const h = await harness();

      const output = await runCallgraph(h, {
        op: "trace_to_symbol",
        filePath: "sample.ts",
        symbol: "funcC",
        toSymbol: "decorate",
        toFile: "sample.ts",
      });

      expect(output).toMatch(/\d+ hops?/);
      expect(output).toContain("funcC");
      expect(output).toContain("decorate");
    });

    test("symbol_not_found is returned as plain text instead of thrown", async () => {
      const h = await harness();
      await runCallgraph(h, { op: "callers", filePath: "sample.ts", symbol: "normalize" });

      const output = await runCallgraph(
        h,
        { op: "callers", filePath: "sample.ts", symbol: "doesNotExist" },
        { allowBuilding: true },
      );

      expect(output).toContain("symbol_not_found");
      expect(output).toContain("doesNotExist");
    });

    test("genuine argument errors throw", async () => {
      const h = await harness();
      const tools = navigationTools(createPluginContext(h));
      const context = createToolContext(h);

      // An empty sentinel on a required path field means the field was not
      // supplied in substance, so the error names the canonical `path` as
      // missing (v0.49 surface rule) rather than the legacy `filePath` alias
      // with the well-formed-Unicode wording.
      await expect(
        tools.aft_callgraph.execute({ op: "callers", filePath: "", symbol: "normalize" }, context),
      ).rejects.toThrow("'path' is required");

      await expect(
        tools.aft_callgraph.execute(
          { op: "not_an_op", filePath: "sample.ts", symbol: "normalize" } as Record<
            string,
            unknown
          >,
          context,
        ),
      ).rejects.toThrow();
    });
  });
}

if (process.env.AFT_OPENCODE_E2E_IMPORT_ONLY !== "1") {
  runCallgraphToolcallSuite();
}
