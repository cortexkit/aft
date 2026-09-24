/// <reference path="../../bun-test.d.ts" />

import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext, ToolDefinition } from "@opencode-ai/plugin";
import { safetyTools } from "../../tools/safety.js";
import type { PluginContext } from "../../types.js";
import { toolResultText } from "../test-helpers.js";
import {
  cleanupHarnesses,
  createHarness,
  type E2EHarness,
  type HarnessFactory,
  type PreparedBinary,
  prepareBinary,
  readTextFile,
} from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = describe.skipIf(!initialBinary.binaryPath);

export function runSafetySuite(
  options: { harnessFactory?: HarnessFactory; name?: string } = {},
): void {
  maybeDescribe(options.name ?? "e2e safety commands", () => {
    let preparedBinary: PreparedBinary = initialBinary;
    const harnesses: E2EHarness[] = [];

    beforeAll(async () => {
      preparedBinary = await prepareBinary();
    });

    afterEach(async () => {
      await cleanupHarnesses(harnesses);
    });

    async function harness(): Promise<E2EHarness> {
      const created = await (options.harnessFactory ?? createHarness)(preparedBinary);
      harnesses.push(created);
      return created;
    }

    const agentCall = (h: E2EHarness, name: string, args: Record<string, unknown> = {}) =>
      h.bridge.toolCall(undefined, name, args);

    test("creates and restores a checkpoint", async () => {
      const h = await harness();
      const filePath = h.path("sample.ts");
      const original = await readTextFile(filePath);

      const checkpoint = await agentCall(h, "safety", {
        op: "checkpoint",
        name: "safe-point",
        files: [filePath],
      });
      await agentCall(h, "write", {
        filePath,
        content: "export const changed = true;\n",
      });
      const restore = await agentCall(h, "safety", { op: "restore", name: "safe-point" });

      expect(checkpoint.success).toBe(true);
      expect(restore.success).toBe(true);
      expect(await readTextFile(filePath)).toBe(original);
    });

    test("undo reverts an edit", async () => {
      const h = await harness();
      const filePath = h.path("with-errors.ts");
      const original = await readTextFile(filePath);

      const edit = await agentCall(h, "edit", {
        filePath,
        oldString: "pending",
        newString: "ready",
        occurrence: 1,
      });
      const undo = await agentCall(h, "safety", { op: "undo", filePath });

      expect(edit.success).toBe(true);
      expect(undo.success).toBe(true);
      expect(await readTextFile(filePath)).toBe(original);
    });

    test("history lists prior snapshots", async () => {
      const h = await harness();
      const filePath = h.path("history.txt");
      await writeFile(filePath, "v1\n");

      await agentCall(h, "write", { filePath, content: "v2\n" });
      await agentCall(h, "write", { filePath, content: "v3\n" });
      const history = await agentCall(h, "safety", { op: "history", filePath });

      expect(history.success).toBe(true);
      expect((history.entries as Array<Record<string, unknown>>).length).toBeGreaterThanOrEqual(2);
    });

    test("multiple undos walk back the stack", async () => {
      const h = await harness();
      const filePath = h.path("undo-stack.txt");
      await writeFile(filePath, "v1\n");

      await agentCall(h, "write", { filePath, content: "v2\n" });
      await agentCall(h, "write", { filePath, content: "v3\n" });
      await agentCall(h, "safety", { op: "undo", filePath });
      expect(await readTextFile(filePath)).toBe("v2\n");

      await agentCall(h, "safety", { op: "undo", filePath });
      expect(await readTextFile(filePath)).toBe("v1\n");
    });

    test("list_checkpoints returns created checkpoints", async () => {
      const h = await harness();
      const filePath = h.path("sample.ts");

      await agentCall(h, "safety", { op: "checkpoint", name: "one", files: [filePath] });
      await agentCall(h, "safety", { op: "checkpoint", name: "two", files: [filePath] });
      const response = await agentCall(h, "safety", { op: "list" });

      expect(response.success).toBe(true);
      const checkpoints = response.checkpoints as Array<Record<string, unknown>>;
      expect(checkpoints.some((checkpoint) => checkpoint.name === "one")).toBe(true);
      expect(checkpoints.some((checkpoint) => checkpoint.name === "two")).toBe(true);
    });

    test("aft_safety returns readable sections for checkpoint, list, restore, undo, and history", async () => {
      const h = await harness();
      const filePath = h.path("safety-toolcall.txt");
      await writeFile(filePath, "v1\n", "utf8");
      const tool = safetyTools(pluginContext(h)).aft_safety;

      const checkpoint = toolResultText(
        await tool.execute(
          { op: "checkpoint", name: "safe-point", filePath: "safety-toolcall.txt" },
          runtime(h),
        ),
      );
      expect(checkpoint).toContain("checkpoint created safe-point");
      expect(checkpoint).toMatch(/files \d+/);
      expect(checkpoint.trim().startsWith("{")).toBe(false);

      await writeFile(filePath, "v2\n", "utf8");
      const restore = toolResultText(
        await tool.execute({ op: "restore", name: "safe-point" }, runtime(h)),
      );
      expect(restore).toContain("checkpoint restored safe-point");
      expect(await readTextFile(filePath)).toBe("v1\n");

      const list = toolResultText(await tool.execute({ op: "list" }, runtime(h)));
      expect(list).toMatch(/\d+ checkpoint\(s\)/);
      expect(list).toContain("safe-point");

      await agentCall(h, "write", { filePath, content: "v3\n" });
      const undo = toolResultText(await tool.execute({ op: "undo", filePath }, runtime(h)));
      expect(undo).toContain("restored");
      expect(await readTextFile(filePath)).toBe("v1\n");

      await agentCall(h, "write", { filePath, content: "v4\n" });
      const history = toolResultText(await tool.execute({ op: "history", filePath }, runtime(h)));
      expect(history).toContain("safety-toolcall.txt");
      expect(history).toMatch(/^1\. /m);
    });

    test("aft_safety asks external-directory and edit permissions for undo and restore", async () => {
      const h = await harness();
      const externalDir = await mkdtemp(join(dirname(h.tempDir), "aft-safety-external-"));
      const externalFile = join(externalDir, "outside.txt");
      const tool = safetyTools(pluginContext(h)).aft_safety;

      try {
        await writeFile(externalFile, "v1\n", "utf8");
        await agentCall(h, "write", { filePath: externalFile, content: "v2\n" });

        let asks: AskCall[] = [];
        const undo = toolResultText(
          await tool.execute(
            { op: "undo", filePath: externalFile },
            runtime(h, recordingAsk(asks)),
          ),
        );
        expect(undo).toContain("restored");
        expect(asks.some((call) => call.permission === "external_directory")).toBe(true);
        expect(asks.some((call) => call.permission === "edit")).toBe(true);
        expect(await readTextFile(externalFile)).toBe("v1\n");

        asks = [];
        await tool.execute(
          { op: "checkpoint", name: "outside-safe", files: [externalFile] },
          runtime(h, recordingAsk(asks)),
        );
        expect(asks.some((call) => call.permission === "external_directory")).toBe(true);

        await writeFile(externalFile, "changed\n", "utf8");
        asks = [];
        const restore = toolResultText(
          await tool.execute(
            { op: "restore", name: "outside-safe" },
            runtime(h, recordingAsk(asks)),
          ),
        );
        expect(restore).toContain("checkpoint restored outside-safe");
        expect(asks.some((call) => call.permission === "external_directory")).toBe(true);
        expect(asks.some((call) => call.permission === "edit")).toBe(true);
        expect(await readTextFile(externalFile)).toBe("v1\n");
      } finally {
        await rm(externalDir, { recursive: true, force: true });
      }
    });

    test("operation undo restores every file from a multi-file delete in one call", async () => {
      // Regression: v0.25 introduced operation-scoped backups. aft_delete
      // files: [a, b, c] writes one op_id; a single `undo` with no `file`
      // param restores all three atomically.
      const h = await harness();
      const fileA = h.path("op-undo-a.txt");
      const fileB = h.path("op-undo-b.txt");
      const fileC = h.path("op-undo-c.txt");

      await writeFile(fileA, "content-a\n");
      await writeFile(fileB, "content-b\n");
      await writeFile(fileC, "content-c\n");

      const deleteResp = await agentCall(h, "delete", {
        files: [fileA, fileB, fileC],
      });
      expect(deleteResp.success).toBe(true);
      expect(deleteResp.complete).toBe(true);
      const { existsSync } = await import("node:fs");
      expect(existsSync(fileA)).toBe(false);
      expect(existsSync(fileB)).toBe(false);
      expect(existsSync(fileC)).toBe(false);

      // Operation undo: no `file` param. Restores everything tagged with the
      // most recent op_id atomically.
      const undoResp = await agentCall(h, "safety", { op: "undo" });
      expect(undoResp.success).toBe(true);
      expect(undoResp.operation).toBe(true);
      expect(undoResp.restored_count).toBe(3);
      expect(await readTextFile(fileA)).toBe("content-a\n");
      expect(await readTextFile(fileB)).toBe("content-b\n");
      expect(await readTextFile(fileC)).toBe("content-c\n");
    });

    test("operation undo restores a recursive directory delete in one call", async () => {
      // Regression: v0.25 added aft_delete recursive: true. Backs every file
      // in the tree under one op_id; single undo restores files AND
      // intermediate parent directories.
      const h = await harness();
      const dir = h.path("op-undo-tree");
      const { mkdir } = await import("node:fs/promises");
      const { existsSync } = await import("node:fs");
      await mkdir(`${dir}/nested`, { recursive: true });
      await writeFile(`${dir}/top.txt`, "top-content\n");
      await writeFile(`${dir}/nested/inner.txt`, "inner-content\n");

      const deleteResp = await agentCall(h, "delete", {
        files: [dir],
        recursive: true,
      });
      expect(deleteResp.success).toBe(true);
      expect(deleteResp.complete).toBe(true);
      const deleted = deleteResp.deleted as Array<Record<string, unknown>>;
      expect(deleted).toHaveLength(1);
      expect(deleted[0]?.is_directory).toBe(true);
      expect(deleted[0]?.files_deleted).toBe(2);
      expect(existsSync(dir)).toBe(false);

      const undoResp = await agentCall(h, "safety", { op: "undo" });
      expect(undoResp.success).toBe(true);
      expect(undoResp.operation).toBe(true);
      expect(undoResp.restored_count).toBe(2);
      // Both files AND their parent directories must be restored.
      expect(await readTextFile(`${dir}/top.txt`)).toBe("top-content\n");
      expect(await readTextFile(`${dir}/nested/inner.txt`)).toBe("inner-content\n");
    });

    test("recursive delete rejects symlinks before touching the filesystem", async () => {
      // Regression: v0.25 guards recursive delete against symlinks (whose
      // canonical target could be outside the tree) and empty directories
      // (which the backup format can't currently restore).
      const h = await harness();
      const dir = h.path("symlink-guard");
      const outside = h.path("symlink-target.txt");
      const { mkdir, symlink } = await import("node:fs/promises");
      const { existsSync } = await import("node:fs");
      await mkdir(dir, { recursive: true });
      await writeFile(`${dir}/real.txt`, "inside\n");
      await writeFile(outside, "outside\n");
      await symlink(outside, `${dir}/link.txt`);

      const resp = await agentCall(h, "delete", {
        files: [dir],
        recursive: true,
      });
      expect(resp.success).toBe(false);
      expect(resp.code).toBe("delete_failed");
      expect(resp.all_failed).toBe(true);
      const skipped = resp.skipped_files as Array<Record<string, unknown>>;
      expect(skipped).toHaveLength(1);
      expect(skipped[0]?.reason as string).toContain("link.txt");
      // The whole tree, the symlink, and the outside target must be untouched.
      expect(existsSync(dir)).toBe(true);
      expect(existsSync(`${dir}/real.txt`)).toBe(true);
      expect(existsSync(`${dir}/link.txt`)).toBe(true);
      expect(await readTextFile(outside)).toBe("outside\n");
    });
  });
}

if (process.env.AFT_OPENCODE_E2E_IMPORT_ONLY !== "1") {
  runSafetySuite();
}

type AskCall = {
  permission?: string;
  patterns?: string[];
  metadata?: Record<string, unknown>;
};

function pluginContext(harness: E2EHarness): PluginContext {
  return {
    pool: { getBridge: () => harness.bridge } as unknown as BridgePool,
    client: {
      lsp: { status: async () => ({ data: [] }) },
      find: { symbols: async () => ({ data: [] }) },
    } as unknown as PluginContext["client"],
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
    messageID: "safety-toolcall-e2e",
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
