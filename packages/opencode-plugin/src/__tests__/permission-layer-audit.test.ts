/// <reference path="../bun-test.d.ts" />
import { afterEach, describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import * as path from "node:path";
import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext, ToolDefinition } from "@opencode-ai/plugin";
import { astTools } from "../tools/ast.js";
import { hoistedTools } from "../tools/hoisted.js";
import {
  _permissionsInternalsForTest,
  assertExternalDirectoryPermission,
} from "../tools/permissions.js";
import { safetyTools } from "../tools/safety.js";
import { searchTools } from "../tools/search.js";
import type { PluginContext } from "../types.js";

type BridgeResponse = Record<string, unknown>;
type SendCall = { command: string; params: Record<string, unknown> };
type AskCall = {
  permission?: string;
  patterns?: string[];
  always?: string[];
  metadata?: Record<string, unknown>;
};

const windowsTest = process.platform === "win32" ? test : test.skip;
let tmpRoot: string | null = null;

afterEach(async () => {
  if (tmpRoot) {
    await rm(tmpRoot, { recursive: true, force: true });
    tmpRoot = null;
  }
});

function createMockClient(): any {
  return {
    lsp: { status: async () => ({ data: [] }) },
    find: { symbols: async () => ({ data: [] }) },
  };
}

function createPluginContext(pool: BridgePool): PluginContext {
  return { pool, client: createMockClient(), config: {} as any, storageDir: "/tmp/aft-test" };
}

function createHarness(
  toolFactory: (ctx: PluginContext) => Record<string, ToolDefinition>,
  sendImpl: (
    command: string,
    params: Record<string, unknown>,
  ) => Promise<BridgeResponse> | BridgeResponse = () => ({ success: true, text: "ok" }),
) {
  const calls: SendCall[] = [];
  const bridge = {
    send: async (command: string, params: Record<string, unknown> = {}) => {
      calls.push({ command, params });
      return await sendImpl(command, params);
    },
  };
  const pool = { getBridge: () => bridge } as unknown as BridgePool;
  return { calls, tools: toolFactory(createPluginContext(pool)) };
}

function createSdkContext(directory: string, ask: ToolContext["ask"]): ToolContext {
  return {
    sessionID: "permission-audit-test",
    messageID: "message-id",
    agent: "test",
    directory,
    worktree: directory,
    abort: new AbortController().signal,
    metadata: () => {},
    ask,
  };
}

function recordingAsk(
  calls: AskCall[],
  deny?: { permission: string; message: string },
): ToolContext["ask"] {
  return (async (input: AskCall) => {
    calls.push(input);
    if (deny && input.permission === deny.permission) {
      throw new Error(deny.message);
    }
  }) as unknown as ToolContext["ask"];
}

async function makeProjectAndExternalDirs(): Promise<{ project: string; external: string }> {
  tmpRoot = await mkdtemp(path.join(tmpdir(), "aft-permission-audit-"));
  const project = path.join(tmpRoot, "project");
  const external = path.join(tmpRoot, "external");
  await mkdir(project, { recursive: true });
  await mkdir(external, { recursive: true });
  return { project, external };
}

function parsePermissionDenied(raw: string): Record<string, unknown> {
  const parsed = JSON.parse(raw) as Record<string, unknown>;
  expect(parsed.success).toBe(false);
  expect(parsed.code).toBe("permission_denied");
  return parsed;
}

describe("permission audit regressions", () => {
  windowsTest("containsPath rejects Windows cross-drive targets as external", async () => {
    const askCalls: AskCall[] = [];
    const ctx = createSdkContext("C:\\repo", recordingAsk(askCalls));

    await assertExternalDirectoryPermission(ctx, "D:\\secret\\file.ts");

    expect(askCalls).toHaveLength(1);
    expect(askCalls[0]?.permission).toBe("external_directory");
    expect(askCalls[0]?.patterns?.[0]).toContain("D:");
  });

  windowsTest(
    "normalizePathPattern preserves single-star and globstar Windows patterns",
    async () => {
      tmpRoot = await mkdtemp(path.join(tmpdir(), "aft-win-pattern-"));
      const normalizedSingle = _permissionsInternalsForTest.normalizePathPattern(`${tmpRoot}\\*`);
      const normalizedGlobstar = _permissionsInternalsForTest.normalizePathPattern(
        `${tmpRoot}\\**`,
      );

      expect(normalizedSingle).toBe(
        path.join(_permissionsInternalsForTest.normalizePathPattern(tmpRoot), "*"),
      );
      expect(normalizedGlobstar).toBe(
        path.join(_permissionsInternalsForTest.normalizePathPattern(tmpRoot), "**"),
      );
    },
  );

  test("ast_grep_replace edit denial returns the permissionDeniedResponse envelope", async () => {
    const { project } = await makeProjectAndExternalDirs();
    const askCalls: AskCall[] = [];
    const sdkCtx = createSdkContext(
      project,
      recordingAsk(askCalls, { permission: "edit", message: "edit denied by policy" }),
    );
    const { calls, tools } = createHarness(astTools);

    const raw = (await tools.ast_grep_replace.execute(
      {
        pattern: "console.log($MSG)",
        rewrite: "logger.info($MSG)",
        lang: "javascript",
        paths: ["."],
      },
      sdkCtx,
    )) as string;

    expect(parsePermissionDenied(raw).message).toBe("edit denied by policy");
    expect(calls).toHaveLength(0);
  });

  test("aft_safety checkpoint asks for explicit external files once per parent", async () => {
    const { project, external } = await makeProjectAndExternalDirs();
    const askCalls: AskCall[] = [];
    const sdkCtx = createSdkContext(project, recordingAsk(askCalls));
    const { calls, tools } = createHarness(safetyTools, () => ({ success: true, name: "snap" }));

    const raw = (await tools.aft_safety.execute(
      {
        op: "checkpoint",
        name: "snap",
        files: [path.join(external, "a.ts"), path.join(external, "b.ts")],
      },
      sdkCtx,
    )) as string;

    expect(JSON.parse(raw).success).toBe(true);
    expect(askCalls.filter((call) => call.permission === "external_directory")).toHaveLength(1);
    expect(calls[0]?.command).toBe("checkpoint");
  });

  test("aft_safety checkpoint external denial returns the permissionDeniedResponse envelope", async () => {
    const { project, external } = await makeProjectAndExternalDirs();
    const askCalls: AskCall[] = [];
    const sdkCtx = createSdkContext(
      project,
      recordingAsk(askCalls, { permission: "external_directory", message: "external denied" }),
    );
    const { calls, tools } = createHarness(safetyTools);

    const raw = (await tools.aft_safety.execute(
      { op: "checkpoint", name: "snap", files: [path.join(external, "a.ts")] },
      sdkCtx,
    )) as string;

    expect(parsePermissionDenied(raw).message).toBe("external denied");
    expect(askCalls[0]?.permission).toBe("external_directory");
    expect(calls).toHaveLength(0);
  });

  test("aft_safety undo still asks edit permission and calls the bridge", async () => {
    const { project } = await makeProjectAndExternalDirs();
    const askCalls: AskCall[] = [];
    const sdkCtx = createSdkContext(project, recordingAsk(askCalls));
    const { calls, tools } = createHarness(safetyTools, () => ({ success: true, backup_id: "b1" }));

    const raw = (await tools.aft_safety.execute(
      { op: "undo", filePath: "inside.ts" },
      sdkCtx,
    )) as string;

    expect(JSON.parse(raw).success).toBe(true);
    expect(askCalls.map((call) => call.permission)).toEqual(["edit"]);
    expect(calls[0]?.command).toBe("undo");
  });

  test("aft_safety undo without filePath calls bridge without file param", async () => {
    const { project } = await makeProjectAndExternalDirs();
    const askCalls: AskCall[] = [];
    const sdkCtx = createSdkContext(project, recordingAsk(askCalls));
    const { calls, tools } = createHarness(safetyTools, () => ({ success: true, operation: true }));

    const raw = (await tools.aft_safety.execute({ op: "undo" }, sdkCtx)) as string;

    expect(JSON.parse(raw).success).toBe(true);
    expect(askCalls).toHaveLength(0);
    expect(calls[0]?.command).toBe("undo");
    expect(calls[0]?.params).not.toHaveProperty("file");
  });

  test("aft_safety undo with filePath still passes file param", async () => {
    const { project } = await makeProjectAndExternalDirs();
    const sdkCtx = createSdkContext(project, recordingAsk([]));
    const { calls, tools } = createHarness(safetyTools, () => ({ success: true, backup_id: "b1" }));

    await tools.aft_safety.execute({ op: "undo", filePath: "inside.ts" }, sdkCtx);

    expect(calls[0]?.command).toBe("undo");
    expect(calls[0]?.params).toMatchObject({ file: "inside.ts" });
  });

  test("ast_grep_search denies external paths before bridge execution", async () => {
    const { project, external } = await makeProjectAndExternalDirs();
    const sdkCtx = createSdkContext(
      project,
      recordingAsk([], { permission: "external_directory", message: "external denied" }),
    );
    const { calls, tools } = createHarness(astTools);

    const raw = (await tools.ast_grep_search.execute(
      { pattern: "console.log($MSG)", lang: "javascript", paths: [external] },
      sdkCtx,
    )) as string;

    expect(parsePermissionDenied(raw).message).toBe("external denied");
    expect(calls).toHaveLength(0);
  });

  test("ast_grep_replace dry-run denies external paths before bridge execution", async () => {
    const { project, external } = await makeProjectAndExternalDirs();
    const sdkCtx = createSdkContext(
      project,
      recordingAsk([], { permission: "external_directory", message: "external denied" }),
    );
    const { calls, tools } = createHarness(astTools);

    const raw = (await tools.ast_grep_replace.execute(
      {
        pattern: "console.log($MSG)",
        rewrite: "logger.info($MSG)",
        lang: "javascript",
        paths: [external],
        dryRun: true,
      },
      sdkCtx,
    )) as string;

    expect(parsePermissionDenied(raw).message).toBe("external denied");
    expect(calls).toHaveLength(0);
  });

  test("grep asks external_directory with directory scope for directory path targets", async () => {
    const { project, external } = await makeProjectAndExternalDirs();
    const askCalls: AskCall[] = [];
    const sdkCtx = createSdkContext(project, recordingAsk(askCalls));
    const { tools } = createHarness(searchTools, () => ({ success: true, text: "ok" }));

    await tools.grep.execute({ pattern: "TODO", path: external }, sdkCtx);

    const externalAsk = askCalls.find((call) => call.permission === "external_directory");
    const expected = path.join(external, "*").split("\\").join("/");
    const widened = path.join(path.dirname(external), "*").split("\\").join("/");
    expect(externalAsk?.patterns).toEqual([expected]);
    expect(externalAsk?.patterns).not.toEqual([widened]);
  });

  test("read permission denial returns the permissionDeniedResponse envelope", async () => {
    const { project } = await makeProjectAndExternalDirs();
    const sdkCtx = createSdkContext(
      project,
      recordingAsk([], { permission: "read", message: "read denied by policy" }),
    );
    const { calls, tools } = createHarness(hoistedTools);

    const raw = (await tools.read.execute({ filePath: "inside.ts" }, sdkCtx)) as string;

    expect(parsePermissionDenied(raw).message).toBe("read denied by policy");
    expect(calls).toHaveLength(0);
  });
});
