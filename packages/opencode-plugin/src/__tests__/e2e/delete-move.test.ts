/// <reference path="../../bun-test.d.ts" />

import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { mkdir, realpath, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext, ToolDefinition } from "@opencode-ai/plugin";
import { hoistedTools } from "../../tools/hoisted.js";
import type { PluginContext } from "../../types.js";
import { toolResultText } from "../test-helpers.js";
import {
  cleanupHarnesses,
  createHarness,
  type E2EHarness,
  type PreparedBinary,
  prepareBinary,
  readTextFile,
} from "./helpers.js";

type AskCall = {
  permission?: string;
  patterns?: string[];
  metadata?: Record<string, unknown>;
};

const initialBinary = await prepareBinary();
const maybeDescribe = describe.skipIf(!initialBinary.binaryPath);

maybeDescribe("e2e delete and move commands", () => {
  let preparedBinary: PreparedBinary = initialBinary;
  const harnesses: E2EHarness[] = [];

  beforeAll(async () => {
    preparedBinary = await prepareBinary();
  });

  afterEach(async () => {
    await cleanupHarnesses(harnesses);
  });

  async function harness(): Promise<E2EHarness> {
    const created = await createHarness(preparedBinary);
    harnesses.push(created);
    return created;
  }

  test("aft_delete returns readable text for success, partial failure, and directories", async () => {
    const h = await harness();
    const tool = hoistedTools(pluginContext(h)).aft_delete;

    const singlePath = h.path("single-delete.txt");
    await writeFile(singlePath, "delete me\n", "utf8");
    const resolvedSinglePath = await realpath(singlePath);
    const askCalls: AskCall[] = [];
    const single = toolResultText(
      await tool.execute({ files: ["single-delete.txt"] }, runtime(h, recordingAsk(askCalls))),
    );
    expect(single).toBe(`Deleted ${resolvedSinglePath}`);
    expect(single.trim().startsWith("{")).toBe(false);
    expect(existsSync(singlePath)).toBe(false);
    expect(askCalls.some((call) => call.permission === "edit")).toBe(true);

    const partialPath = h.path("partial-delete.txt");
    await writeFile(partialPath, "delete me too\n", "utf8");
    const partial = toolResultText(
      await tool.execute({ files: ["partial-delete.txt", "missing-delete.txt"] }, runtime(h)),
    );
    expect(partial).toBe("Deleted 1/2 file(s)");
    expect(existsSync(partialPath)).toBe(false);

    await expect(tool.execute({ files: ["fully-missing.txt"] }, runtime(h))).rejects.toThrow(
      "delete failed for all 1 file(s)",
    );

    const dirWithoutRecursive = h.path("dir-without-recursive");
    await mkdir(dirWithoutRecursive);
    await writeFile(join(dirWithoutRecursive, "child.txt"), "child\n", "utf8");
    await expect(tool.execute({ files: ["dir-without-recursive"] }, runtime(h))).rejects.toThrow(
      "Pass recursive: true",
    );
    expect(existsSync(dirWithoutRecursive)).toBe(true);

    const recursiveDir = h.path("recursive-delete");
    await mkdir(recursiveDir);
    await writeFile(join(recursiveDir, "child.txt"), "child\n", "utf8");
    const resolvedRecursiveDir = await realpath(recursiveDir);
    const recursive = toolResultText(
      await tool.execute({ files: ["recursive-delete"], recursive: true }, runtime(h)),
    );
    expect(recursive).toBe(`Deleted ${resolvedRecursiveDir}`);
    expect(existsSync(recursiveDir)).toBe(false);
  });

  test("aft_move returns readable agent-typed paths and relocates the file", async () => {
    const h = await harness();
    const tool = hoistedTools(pluginContext(h)).aft_move;

    await writeFile(h.path("move-source.txt"), "move me\n", "utf8");
    const moved = toolResultText(
      await tool.execute(
        { filePath: "move-source.txt", destination: "nested/move-dest.txt" },
        runtime(h),
      ),
    );

    expect(moved).toBe("Moved move-source.txt → nested/move-dest.txt");
    expect(moved.trim().startsWith("{")).toBe(false);
    expect(existsSync(h.path("move-source.txt"))).toBe(false);
    expect(await readTextFile(h.path("nested/move-dest.txt"))).toBe("move me\n");
  });

  test("aft_move asks external-directory and edit permissions", async () => {
    const h = await harness();
    const tool = hoistedTools(pluginContext(h)).aft_move;
    const external = mkdtempSync(join(dirname(h.tempDir), "aft-move-external-"));
    try {
      const externalSource = join(external, "external-source.txt");
      const externalDest = join(external, "external-dest.txt");
      await writeFile(externalSource, "external\n", "utf8");

      const asks: AskCall[] = [];
      const output = toolResultText(
        await tool.execute(
          { filePath: externalSource, destination: externalDest },
          runtime(h, recordingAsk(asks)),
        ),
      );

      expect(output).toBe(`Moved ${externalSource} → ${externalDest}`);
      expect(asks.some((call) => call.permission === "external_directory")).toBe(true);
      expect(asks.some((call) => call.permission === "edit")).toBe(true);
      expect(existsSync(externalSource)).toBe(false);
      expect(await readTextFile(externalDest)).toBe("external\n");
    } finally {
      rmSync(external, { recursive: true, force: true });
    }
  });
});

function pluginContext(harness: E2EHarness): PluginContext {
  const pool = {
    getBridge: () => harness.bridge,
  } as unknown as BridgePool;
  return {
    pool,
    client: { lsp: {}, find: {} } as PluginContext["client"],
    config: {
      disabled_tools: [],
      lsp: { diagnostics_on_edit: false },
    } as PluginContext["config"],
    storageDir: harness.path(".storage"),
  };
}

function runtime(
  harness: E2EHarness,
  ask: ToolContext["ask"] = async () => undefined,
): Parameters<ToolDefinition["execute"]>[1] {
  return {
    directory: harness.tempDir,
    worktree: harness.tempDir,
    sessionID: undefined,
    messageID: "delete-move-toolcall-e2e",
    agent: "test",
    abort: new AbortController().signal,
    metadata: () => {},
    ask,
  } as unknown as Parameters<ToolDefinition["execute"]>[1];
}

function recordingAsk(calls: AskCall[]): ToolContext["ask"] {
  return (async (input: AskCall) => {
    calls.push(input);
  }) as unknown as ToolContext["ask"];
}
