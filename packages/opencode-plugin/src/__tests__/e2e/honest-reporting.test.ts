/// <reference path="../../bun-test.d.ts" />

import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { astTools } from "../../tools/ast.js";
import { importTools } from "../../tools/imports.js";
import { lspTools } from "../../tools/lsp.js";
import { readingTools } from "../../tools/reading.js";
import type { PluginContext } from "../../types.js";
import { noopAsk } from "../test-helpers";
import {
  cleanupHarnesses,
  createHarness,
  type E2EHarness,
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

function createPluginContext(pool: BridgePool, storageDir: string): PluginContext {
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

maybeDescribe("e2e honest reporting surfaces", () => {
  let preparedBinary: PreparedBinary = initialBinary;
  const harnesses: E2EHarness[] = [];
  const pools: BridgePool[] = [];

  beforeAll(async () => {
    preparedBinary = await prepareBinary();
  });

  afterEach(async () => {
    await Promise.allSettled(pools.splice(0, pools.length).map((pool) => pool.shutdown()));
    await cleanupHarnesses(harnesses);
  });

  async function toolHarness() {
    const h = await createHarness(preparedBinary, { fixtureNames: [], timeoutMs: 20_000 });
    harnesses.push(h);
    const storageDir = join(h.tempDir, ".storage");
    const pool = new BridgePool(
      h.binaryPath,
      { timeoutMs: 20_000 },
      {
        storage_dir: storageDir,
        harness: "opencode",
      },
    );
    pools.push(pool);
    const pluginContext = createPluginContext(pool, storageDir);
    return {
      h,
      sdkCtx: createSdkContext(h.tempDir),
      tools: {
        ...readingTools(pluginContext),
        ...astTools(pluginContext),
        ...importTools(pluginContext),
        ...lspTools(pluginContext),
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

    const output = await tools.aft_outline.execute({ target: "outline-small" }, sdkCtx);
    const response = JSON.parse(output) as Record<string, unknown>;

    expect(response.complete).toBe(true);
    expect(response.walk_truncated).toBe(false);
    const skipped = response.skipped_files as Array<{ file: string; reason: string }>;
    expect(skipped).toHaveLength(1);
    expect(skipped[0].file).toMatch(/outline-small[/\\]bad\.ts$/);
    expect(skipped[0].reason).toBe("parse_error");
    expect(String(response.text)).toContain("good.ts");
    expect(String(response.text)).toContain("good");
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

    const output = await tools.aft_outline.execute({ target: "outline-large" }, sdkCtx);
    const response = JSON.parse(output) as Record<string, unknown>;

    expect(response.complete).toBe(false);
    expect(response.walk_truncated).toBe(true);
    expect(Array.isArray(response.skipped_files)).toBe(true);
    expect(String(response.text)).toContain("file-000.ts");
  });

  test("aft_outline single file target keeps text output behavior", async () => {
    const { h, tools, sdkCtx } = await toolHarness();
    await writeFile(h.path("single.ts"), "export function single() { return 1; }\n", "utf8");

    const output = await tools.aft_outline.execute({ target: "single.ts" }, sdkCtx);

    expect(() => JSON.parse(output)).toThrow();
    expect(output).toContain("single.ts");
    expect(output).toContain("single");
  });

  test("aft_outline array target keeps multi-file text output behavior", async () => {
    const { h, tools, sdkCtx } = await toolHarness();
    await writeFile(h.path("array-a.ts"), "export function arrayA() { return 1; }\n", "utf8");
    await writeFile(h.path("array-b.ts"), "export function arrayB() { return 2; }\n", "utf8");

    const output = await tools.aft_outline.execute(
      { target: [h.path("array-a.ts"), h.path("array-b.ts")] },
      sdkCtx,
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

    const output = await tools.ast_grep_search.execute(
      { pattern: "console.log($MSG)", lang: "typescript", paths: ["python-only"] },
      sdkCtx,
    );

    expect(output).toContain("No files matched the scope");
    expect(output).not.toContain("No matches found (searched");
  });

  test("ast_grep_search reports searched-zero-hit result for valid scope", async () => {
    const { h, tools, sdkCtx } = await toolHarness();
    await writeFile(h.path("valid.ts"), "export const value = 1;\n", "utf8");

    const output = await tools.ast_grep_search.execute(
      { pattern: "console.log($MSG)", lang: "typescript", paths: ["valid.ts"] },
      sdkCtx,
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

    const output = await tools.aft_import.execute(
      { op: "remove", filePath: "imports.ts", module: "missing-pkg" },
      sdkCtx,
    );
    const response = JSON.parse(output) as Record<string, unknown>;

    expect(response.success).toBe(true);
    expect(response.removed).toBe(false);
    expect(String(response.message ?? response.reason ?? "")).toMatch(
      /not[_ ]found|absent|missing/i,
    );
  });

  test("lsp_diagnostics discloses when no server checked a file", async () => {
    const { h, tools, sdkCtx } = await toolHarness();
    await writeFile(h.path("notes.txt"), "plain text\n", "utf8");

    const output = await tools.lsp_diagnostics.execute({ filePath: "notes.txt" }, sdkCtx);
    const response = JSON.parse(output) as Record<string, unknown>;

    expect(response.success).toBe(true);
    expect(response.complete).toBe(true);
    expect(response.total).toBe(0);
    expect(response.lsp_servers_used).toEqual([]);
    expect(String(response.note ?? "")).toMatch(/nothing was checked|no lsp server/i);
  });
});
