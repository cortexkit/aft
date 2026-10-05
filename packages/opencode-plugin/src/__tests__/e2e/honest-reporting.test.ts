/// <reference path="../../bun-test.d.ts" />

import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import { mkdir, realpath, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { type AftTransportPool, BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { astTools } from "../../tools/ast.js";
import { importTools } from "../../tools/imports.js";
import { readingTools } from "../../tools/reading.js";
import type { PluginContext } from "../../types.js";
import { noopAsk, toolResultText } from "../test-helpers";
import {
  cleanupHarnesses,
  createHarness,
  type E2EHarness,
  type HarnessFactory,
  harnessPool,
  type PreparedBinary,
  prepareBinary,
} from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = describe.skipIf(!initialBinary.binaryPath);

function createMockClient(): any {
  return {
    lsp: { status: async () => ({ data: [] }) },
    find: { symbols: async () => ({ data: [] }) },
  };
}

function createPluginContext(pool: AftTransportPool, storageDir: string): PluginContext {
  return {
    pool,
    client: createMockClient(),
    config: {} as PluginContext["config"],
    storageDir,
  };
}

function createSdkContext(directory: string): ToolContext {
  return {
    sessionID: "honest-reporting-e2e",
    messageID: "honest-reporting-message",
    agent: "test",
    directory,
    worktree: directory,
    abort: new AbortController().signal,
    metadata: () => {},
    ask: noopAsk,
  };
}

async function waitForBridgeReady(pool: AftTransportPool, directory: string): Promise<void> {
  const response = await pool.getBridge(directory).send("ping", {
    session_id: "honest-reporting-e2e",
  });
  if (response.success !== true) {
    throw new Error(
      `bridge readiness check failed: ${String(response.message ?? response.code ?? "unknown")}`,
    );
  }
}

export function runHonestReportingSuite(
  options: { harnessFactory?: HarnessFactory; name?: string } = {},
): void {
  maybeDescribe(options.name ?? "e2e honest reporting surfaces", () => {
    let preparedBinary: PreparedBinary = initialBinary;
    const harnesses: E2EHarness[] = [];
    const pools: AftTransportPool[] = [];

    beforeAll(async () => {
      preparedBinary = await prepareBinary();
    });

    afterEach(async () => {
      await Promise.allSettled(pools.splice(0, pools.length).map((pool) => pool.shutdown()));
      await cleanupHarnesses(harnesses);
    });

    async function toolHarness() {
      const h = await (options.harnessFactory ?? createHarness)(preparedBinary, {
        fixtureNames: [],
        timeoutMs: 20_000,
      });
      harnesses.push(h);
      const storageDir = join(h.tempDir, ".storage");
      const pool =
        h.transport === "subc"
          ? harnessPool(h)
          : new BridgePool(
              h.binaryPath,
              { timeoutMs: 20_000 },
              { storage_dir: storageDir, harness: "opencode" },
            );
      if (h.transport !== "subc") pools.push(pool);
      const pluginContext = createPluginContext(pool, storageDir);
      if (h.transport !== "subc") await waitForBridgeReady(pool, h.tempDir);
      return {
        h,
        sdkCtx: createSdkContext(h.tempDir),
        tools: {
          ...readingTools(pluginContext),
          ...astTools(pluginContext),
          ...importTools(pluginContext),
        },
      };
    }

    test("aft_outline directory mode returns complete true below walk cap", async () => {
      const { h, tools, sdkCtx } = await toolHarness();
      await mkdir(h.path("outline-small"), { recursive: true });
      await writeFile(
        h.path("outline-small", "good.ts"),
        "export function good() { return 1; }\n",
        "utf8",
      );
      await writeFile(h.path("outline-small", "bad.ts"), "export function bad( {\n", "utf8");

      const output = toolResultText(
        await tools.aft_outline.execute({ target: "outline-small" }, sdkCtx),
      );

      // Directory outlines are plain text; honesty data arrives as footers.
      expect(() => JSON.parse(output)).toThrow();
      expect(output).toContain("good.ts");
      expect(output).toContain("good");
      expect(output).not.toContain("⚠ Partial result");
      expect(output).toContain("Skipped 1 file(s):");
      expect(output).toMatch(/bad\.ts — parse_error/);
    });

    test("aft_outline directory mode returns complete false when Rust walk truncates", async () => {
      const { h, tools, sdkCtx } = await toolHarness();
      await mkdir(h.path("outline-large"), { recursive: true });
      for (let index = 0; index < 205; index += 1) {
        await writeFile(
          h.path("outline-large", `file-${String(index).padStart(3, "0")}.ts`),
          `export const value${index} = ${index};\n`,
          "utf8",
        );
      }

      const output = toolResultText(
        await tools.aft_outline.execute({ target: "outline-large" }, sdkCtx),
      );

      // Truncation is disclosed in the plain-text partial-result footer.
      expect(() => JSON.parse(output)).toThrow();
      expect(output).toContain("file-000.ts");
      expect(output).toContain(" files (walk) · narrow: path");
      expect(output.trimEnd().split("\n").at(-1)).toMatch(
        /^shown \d+ of ≥200 files \(walk\) · narrow: path$/,
      );
    });

    test("aft_outline single file target keeps text output behavior", async () => {
      const { h, tools, sdkCtx } = await toolHarness();
      await writeFile(h.path("single.ts"), "export function single() { return 1; }\n", "utf8");

      const output = toolResultText(
        await tools.aft_outline.execute({ target: "single.ts" }, sdkCtx),
      );

      expect(() => JSON.parse(output)).toThrow();
      expect(output).toContain("single.ts");
      expect(output).toContain("single");
    });

    test("aft_outline array target keeps multi-file text output behavior", async () => {
      const { h, tools, sdkCtx } = await toolHarness();
      await writeFile(h.path("array-a.ts"), "export function arrayA() { return 1; }\n", "utf8");
      await writeFile(h.path("array-b.ts"), "export function arrayB() { return 2; }\n", "utf8");

      const output = toolResultText(
        await tools.aft_outline.execute(
          { target: [h.path("array-a.ts"), h.path("array-b.ts")] },
          sdkCtx,
        ),
      );

      expect(() => JSON.parse(output)).toThrow();
      expect(output).toContain("array-a.ts");
      expect(output).toContain("array-b.ts");
      expect(output).toContain("arrayA");
      expect(output).toContain("arrayB");
    });

    test("ast_grep_search reports no_files_matched_scope separately from zero hits", async () => {
      const { h, tools, sdkCtx } = await toolHarness();
      await mkdir(h.path("python-only"), { recursive: true });
      await writeFile(h.path("python-only", "sample.py"), "print('hello')\n", "utf8");

      const output = toolResultText(
        await tools.ast_grep_search.execute(
          { pattern: "console.log($MSG)", lang: "typescript", paths: ["python-only"] },
          sdkCtx,
        ),
      );

      expect(output).toContain("No files matched the scope");
      expect(output).not.toContain("No matches found (searched");
    });

    test("ast_grep_search reports searched-zero-hit result for valid scope", async () => {
      const { h, tools, sdkCtx } = await toolHarness();
      await writeFile(h.path("valid.ts"), "export const value = 1;\n", "utf8");

      const output = toolResultText(
        await tools.ast_grep_search.execute(
          { pattern: "console.log($MSG)", lang: "typescript", paths: ["valid.ts"] },
          sdkCtx,
        ),
      );

      expect(output).toContain("No matches found (searched 1 files)");
      expect(output).not.toContain("No files matched the scope");
    });

    test("aft_import remove reports successful no-op when import is absent", async () => {
      const { h, tools, sdkCtx } = await toolHarness();
      await writeFile(
        h.path("imports.ts"),
        "import { present } from 'pkg';\nconsole.log(present);\n",
        "utf8",
      );

      const output = toolResultText(
        await tools.aft_import.execute(
          { op: "remove", filePath: "imports.ts", module: "missing-pkg" },
          sdkCtx,
        ),
      );
      expect(() => JSON.parse(output)).toThrow();
      expect(output).toContain("not present missing-pkg");
      expect(output).toContain(`file ${await realpath(h.path("imports.ts"))}`);
      expect(output).toContain("scope entire import");
    });
  });
}

if (process.env.AFT_OPENCODE_E2E_IMPORT_ONLY !== "1") {
  runHonestReportingSuite();
}
