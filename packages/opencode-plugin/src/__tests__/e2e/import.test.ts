/// <reference path="../../bun-test.d.ts" />

import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { realpath, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext, ToolDefinition } from "@opencode-ai/plugin";
import { importTools } from "../../tools/imports.js";
import type { PluginContext } from "../../types.js";
import { toolResultText } from "../test-helpers";
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

maybeDescribe("e2e import commands", () => {
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

  test("adds an import", async () => {
    const h = await harness();
    const filePath = h.path("imports.ts");

    const response = await h.bridge.send("add_import", {
      file: filePath,
      module: "lodash",
      names: ["debounce"],
    });

    expect(response.success).toBe(true);
    expect(response.added).toBe(true);
    expect(await readTextFile(filePath)).toContain("import { debounce } from 'lodash';");
  });

  test("removes an import", async () => {
    const h = await harness();
    const filePath = h.path("imports.ts");

    const response = await h.bridge.send("remove_import", {
      file: filePath,
      module: "zod",
    });

    expect(response.success).toBe(true);
    expect(await readTextFile(filePath)).not.toContain('from "zod"');
  });

  test("organizes imports", async () => {
    const h = await harness();
    const filePath = h.path("imports.ts");

    await h.bridge.send("add_import", {
      file: filePath,
      module: "axios",
      default_import: "axios",
    });
    const response = await h.bridge.send("organize_imports", { file: filePath });

    expect(response.success).toBe(true);
    const content = await readTextFile(filePath);
    const axiosIndex = content.indexOf("import axios from 'axios';");
    const parseIndex = content.indexOf("import { parse } from 'jsonc-parser';");
    expect(axiosIndex).toBeGreaterThanOrEqual(0);
    expect(parseIndex).toBeGreaterThanOrEqual(0);
    expect(axiosIndex).toBeLessThan(parseIndex);
  });

  test("aft_import uses tool_call formatting for add, remove, and organize", async () => {
    const h = await harness();
    const filePath = h.path("imports.ts");
    const resolvedFilePath = await realpath(filePath);
    const tool = importTools(pluginContext(h)).aft_import;

    const added = toolResultText(
      await tool.execute(
        { op: "add", filePath: "imports.ts", module: "lodash", names: ["debounce"] },
        runtime(h),
      ),
    );
    expect(added).toContain("added lodash");
    expect(added).toContain(`file ${resolvedFilePath}`);
    expect(added).toContain("group ");
    expect(added.trim().startsWith("{")).toBe(false);
    expect(await readTextFile(filePath)).toContain("import { debounce } from 'lodash';");

    const alreadyPresent = toolResultText(
      await tool.execute(
        { op: "add", filePath: "imports.ts", module: "lodash", names: ["debounce"] },
        runtime(h),
      ),
    );
    expect(alreadyPresent).toContain("already present lodash");
    expect(alreadyPresent).toContain("group —");

    const removed = toolResultText(
      await tool.execute(
        { op: "remove", filePath: "imports.ts", module: "lodash", removeName: "debounce" },
        runtime(h),
      ),
    );
    expect(removed).toContain("removed lodash");
    expect(removed).toContain("name debounce");
    expect(await readTextFile(filePath)).not.toContain("lodash");

    const notPresent = toolResultText(
      await tool.execute(
        { op: "remove", filePath: "imports.ts", module: "missing-pkg" },
        runtime(h),
      ),
    );
    expect(notPresent).toContain("not present missing-pkg");
    expect(notPresent).toContain("scope entire import");

    await writeFile(
      filePath,
      "import { z } from 'zod';\nimport { parse } from 'jsonc-parser';\nconsole.log(z, parse);\n",
      "utf8",
    );
    const organized = toolResultText(
      await tool.execute({ op: "organize", filePath: "imports.ts" }, runtime(h)),
    );
    expect(organized).toContain(`organized ${resolvedFilePath}`);
    expect(organized).toContain("groups ");
    expect(organized).toMatch(/duplicates removed \d+/);
  });

  test("aft_import validates module before dispatch", async () => {
    const h = await harness();
    const tool = importTools(pluginContext(h)).aft_import;

    await expect(tool.execute({ op: "add", filePath: "imports.ts" }, runtime(h))).rejects.toThrow(
      "'module' is required for 'add' op",
    );
  });

  test("aft_import asks external-directory and edit permissions", async () => {
    const h = await harness();
    const tool = importTools(pluginContext(h)).aft_import;
    const external = mkdtempSync(join(dirname(h.tempDir), "aft-import-external-"));
    try {
      const externalFile = join(external, "external.ts");
      await writeFile(externalFile, "export const value = 1;\n", "utf8");

      const asks: AskCall[] = [];
      const output = toolResultText(
        await tool.execute(
          { op: "add", filePath: externalFile, module: "zod", names: ["z"] },
          runtime(h, recordingAsk(asks)),
        ),
      );

      expect(output).toContain("added zod");
      expect(asks.some((call) => call.permission === "external_directory")).toBe(true);
      expect(asks.some((call) => call.permission === "edit")).toBe(true);
      expect(await readTextFile(externalFile)).toContain("import { z } from 'zod';");
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
    messageID: "import-toolcall-e2e",
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
