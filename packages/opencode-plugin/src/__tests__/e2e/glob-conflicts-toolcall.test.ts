/// <reference path="../../bun-test.d.ts" />

import { afterEach, describe, expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { mkdir, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext, ToolDefinition } from "@opencode-ai/plugin";
import { conflictTools } from "../../tools/conflicts.js";
import { searchTools } from "../../tools/search.js";
import type { PluginContext } from "../../types.js";
import { cleanupHarnesses, createHarness, type E2EHarness, prepareBinary } from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = describe.skipIf(!initialBinary.binaryPath);

type AskCall = {
  permission?: string;
  patterns?: string[];
  metadata?: Record<string, unknown>;
};

maybeDescribe("glob/conflicts tool_call e2e", () => {
  const harnesses: E2EHarness[] = [];

  afterEach(async () => {
    await cleanupHarnesses(harnesses);
  });

  async function harness(): Promise<E2EHarness> {
    const created = await createHarness(initialBinary, {
      fixtureNames: [],
      tempPrefix: "aft-plugin-glob-conflicts-",
    });
    harnesses.push(created);
    return created;
  }

  test("glob returns normal, no-match, and missing-path scoped results", async () => {
    const h = await harness();
    await writeGlobFixture(h.tempDir);
    const glob = searchTools(pluginContext(h)).glob;

    const normal = await glob.execute({ pattern: "**/*.ts" }, runtime(h));
    expect(normal).toContain("2 files matching **/*.ts");
    expect(normal).toContain("src/main.ts");
    expect(normal).toContain("scripts/helper.ts");

    const noMatch = await glob.execute({ pattern: "**/*.zzz" }, runtime(h));
    expect(noMatch).toContain("0 files matching **/*.zzz");

    const missing = h.path("missing-dir");
    const scoped = await glob.execute(
      { pattern: "**/*.ts", path: `${h.path("src")} ${missing}` },
      runtime(h),
    );
    expect(scoped).toContain("1 file matching **/*.ts");
    expect(scoped).toContain("src/main.ts");
    expect(scoped).toContain(`Skipped 1 path not found: ${missing}`);
  });

  test("glob asks before searching an external directory", async () => {
    const h = await harness();
    await writeGlobFixture(h.tempDir);
    const external = mkdtempSync(join(dirname(h.tempDir), "aft-glob-external-"));
    try {
      await writeGlobFixture(external);
      const askCalls: AskCall[] = [];
      const output = await searchTools(pluginContext(h)).glob.execute(
        { pattern: "**/*.ts", path: external },
        runtime(h, recordingAsk(askCalls)),
      );

      expect(output).toContain("2 files matching **/*.ts");
      expect(askCalls.some((call) => call.permission === "external_directory")).toBe(true);
    } finally {
      rmSync(external, { recursive: true, force: true });
    }
  });

  test("conflicts reports a clean repository", async () => {
    const h = await harness();
    createCleanRepo(h.tempDir);

    const output = await conflictTools(pluginContext(h)).aft_conflicts.execute({}, runtime(h));

    expect(output).toContain("No merge conflicts found.");
    expect(output).toContain("Checked repo root:");
  });

  test("conflicts reports line-numbered merge conflict regions", async () => {
    const h = await harness();
    createConflictedRepo(h.tempDir);

    const output = await conflictTools(pluginContext(h)).aft_conflicts.execute({}, runtime(h));

    expect(output).toContain("1 file, 1 conflict");
    expect(output).toContain("── conflict.txt [1 conflict] ──");
    expect(output).toMatch(/\d+: <<<<<<< HEAD/);
    expect(output).toMatch(/\d+: =======/);
    expect(output).toMatch(/\d+: >>>>>>> ours/);
  });

  test("conflicts asks before inspecting another repository", async () => {
    const h = await harness();
    await writeGlobFixture(h.tempDir);
    const external = mkdtempSync(join(dirname(h.tempDir), "aft-conflicts-external-"));
    try {
      createCleanRepo(external);
      const askCalls: AskCall[] = [];
      const output = await conflictTools(pluginContext(h)).aft_conflicts.execute(
        { path: external },
        runtime(h, recordingAsk(askCalls)),
      );

      expect(output).toContain("No merge conflicts found.");
      expect(output).toContain("Checked repo root:");
      expect(askCalls.some((call) => call.permission === "external_directory")).toBe(true);
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
    messageID: "message-id",
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

async function writeGlobFixture(root: string): Promise<void> {
  await mkdir(join(root, "src"), { recursive: true });
  await mkdir(join(root, "scripts"), { recursive: true });
  await writeFile(join(root, "src", "main.ts"), "export const main = 1;\n", "utf8");
  await writeFile(join(root, "scripts", "helper.ts"), "export const helper = 1;\n", "utf8");
  await writeFile(join(root, "src", "readme.md"), "# docs\n", "utf8");
}

function createCleanRepo(root: string): void {
  run("git", ["init"], root);
  run("git", ["checkout", "-b", "main"], root);
  run("git", ["config", "user.name", "AFT E2E"], root);
  run("git", ["config", "user.email", "aft-e2e@example.com"], root);
  writeFileSync(join(root, "clean.txt"), "clean\n");
  run("git", ["add", "."], root);
  run("git", ["commit", "-m", "clean"], root);
}

function createConflictedRepo(root: string): void {
  run("git", ["init"], root);
  run("git", ["checkout", "-b", "main"], root);
  run("git", ["config", "user.name", "AFT E2E"], root);
  run("git", ["config", "user.email", "aft-e2e@example.com"], root);
  writeFileSync(join(root, "conflict.txt"), "before\nbase\nafter\n");
  run("git", ["add", "."], root);
  run("git", ["commit", "-m", "base"], root);
  run("git", ["checkout", "-b", "ours"], root);
  writeFileSync(join(root, "conflict.txt"), "before\nours\nafter\n");
  run("git", ["commit", "-am", "ours"], root);
  run("git", ["checkout", "main"], root);
  run("git", ["checkout", "-b", "theirs"], root);
  writeFileSync(join(root, "conflict.txt"), "before\ntheirs\nafter\n");
  run("git", ["commit", "-am", "theirs"], root);
  run("git", ["merge", "ours"], root, { allowFailure: true });
}

function run(
  command: string,
  args: string[],
  cwd: string,
  options: { allowFailure?: boolean } = {},
): void {
  const result = spawnSync(command, args, {
    cwd,
    encoding: "utf8",
    env: {
      ...process.env,
      GIT_AUTHOR_NAME: "AFT E2E",
      GIT_AUTHOR_EMAIL: "aft-e2e@example.com",
      GIT_COMMITTER_NAME: "AFT E2E",
      GIT_COMMITTER_EMAIL: "aft-e2e@example.com",
    },
    stdio: ["ignore", "pipe", "pipe"],
  });

  if (result.error) {
    throw result.error;
  }
  if (!options.allowFailure && result.status !== 0) {
    throw new Error(result.stderr || result.stdout || `${command} failed`);
  }
}
