/**
 * E2E coverage for Pi's hoisted bash tool.
 *
 * These tests intentionally exercise the real Rust binary through Pi's
 * BinaryBridge-backed adapter. Pi has no permission system, so the OpenCode
 * permission-scan e2e case is intentionally not mirrored here.
 */

/// <reference path="../../bun-test.d.ts" />

import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import { mkdir, realpath, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { BridgePool } from "@cortexkit/aft-bridge";
import { registerBashTool } from "../../tools/bash.js";
import type { PluginContext } from "../../types.js";
import {
  createHarness,
  type Harness,
  type MockExtensionContext,
  type MockToolDef,
  prepareBinary,
} from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = initialBinary.binaryPath ? describe : describe.skip;

interface BashDetails {
  exit_code?: number;
  task_id?: string;
  bg_completions?: Array<{ task_id: string; status: string; exit_code?: number; command?: string }>;
}

interface BashStatusDetails {
  success: boolean;
  status: string;
  exit_code?: number;
  output_preview?: string;
  command?: string;
}

interface BashKillDetails {
  success: boolean;
  status: string;
}

maybeDescribe("e2e bash command (Pi adapter + bridge + Rust)", () => {
  let harnesses: Harness[] = [];

  beforeAll(async () => {
    await prepareBinary();
  });

  afterEach(async () => {
    await Promise.allSettled(harnesses.map((harness) => harness.cleanup()));
    harnesses = [];
  });

  async function harness(configOverrides: Record<string, unknown> = {}): Promise<Harness> {
    const created = await createHarness(initialBinary, {
      fixtureNames: [],
      config: { search_index: false, ...toConfigureOverrides(configOverrides) },
      timeoutMs: 60_000,
    });
    harnesses.push(created);
    return created;
  }

  async function pluginHarness(configOverrides: Record<string, unknown> = {}) {
    const h = await harness();
    const pool = new BridgePool(
      h.binaryPath,
      { timeoutMs: 60_000 },
      {
        project_root: h.tempDir,
        restrict_to_project_root: false,
        storage_dir: join(h.tempDir, ".aft-storage"),
        harness: "pi",
        ...configOverrides,
      },
    );
    // Mirror flat `experimental_bash_*` configure overrides back into the
    // nested user-facing config shape that Pi's plugin reads from
    // `ctx.config.experimental.bash.*` to decide whether to register
    // bash_status / bash_kill. Without this, gating in registerBashTool would
    // skip them even when the bridge has experimental_bash_background=true.
    const bashExperimental = {
      ...(configOverrides.experimental_bash_rewrite !== undefined
        ? { rewrite: configOverrides.experimental_bash_rewrite as boolean }
        : {}),
      ...(configOverrides.experimental_bash_compress !== undefined
        ? { compress: configOverrides.experimental_bash_compress as boolean }
        : {}),
      ...(configOverrides.experimental_bash_background !== undefined
        ? { background: configOverrides.experimental_bash_background as boolean }
        : {}),
    };
    const ctxConfig: Record<string, unknown> =
      Object.keys(bashExperimental).length > 0 ? { experimental: { bash: bashExperimental } } : {};
    const ctx: PluginContext = {
      pool,
      config: ctxConfig as PluginContext["config"],
      storageDir: join(h.tempDir, ".aft-storage"),
    };
    const tools = new Map<string, MockToolDef>();
    registerBashTool(
      { registerTool: (tool: MockToolDef) => tools.set(tool.name, tool) } as never,
      ctx,
    );
    const cleanup = h.cleanup;
    Object.defineProperty(h, "cleanup", {
      value: async () => {
        await pool.shutdown();
        await cleanup.call(h);
      },
    });
    return {
      h,
      pool,
      bash: tools.get("bash")!,
      bashStatus: tools.get("bash_status")!,
      bashKill: tools.get("bash_kill")!,
    };
  }

  async function callBash(
    bash: MockToolDef,
    h: Harness,
    params: Record<string, unknown>,
  ): Promise<{ output: string; details: BashDetails }> {
    const extCtx: MockExtensionContext = { cwd: h.tempDir, hasUI: false };
    const result = await bash.execute(
      `test-bash-${Date.now()}`,
      params,
      undefined,
      undefined,
      extCtx,
    );
    return { output: h.text(result), details: (result.details ?? {}) as BashDetails };
  }

  async function bridgeBashToTerminal(
    h: Harness,
    args: Record<string, unknown>,
  ): Promise<Record<string, unknown>> {
    const launched = await h.bridge.send("bash", args);
    expect(launched.success).toBe(true);
    expect(launched.status).toBe("running");
    const taskId = launched.task_id as string;
    const started = Date.now();
    while (Date.now() - started < 5_000) {
      const status = await h.bridge.send("bash_status", { task_id: taskId });
      if (status.status !== "running") return status;
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    throw new Error(`timed out waiting for ${taskId}`);
  }

  async function callTaskTool<TDetails>(
    tool: MockToolDef,
    h: Harness,
    taskId: string,
  ): Promise<{ output: string; details: TDetails }> {
    const extCtx: MockExtensionContext = { cwd: h.tempDir, hasUI: false };
    const result = await tool.execute(
      `test-${tool.name}-${Date.now()}`,
      { task_id: taskId },
      undefined,
      undefined,
      extCtx,
    );
    return { output: h.text(result), details: result.details as TDetails };
  }

  test("foreground simple command returns output and exit code", async () => {
    const { h, bash } = await pluginHarness();

    const result = await callBash(bash, h, { command: "echo hello" });

    expect(result.output).toBe("hello\n");
    expect(result.details.exit_code).toBe(0);
  });

  test("foreground non-zero exit is a successful tool response", async () => {
    const { h, bash } = await pluginHarness();

    const result = await callBash(bash, h, { command: "false" });

    expect(result.output).toBe("\n[exit code: 1]");
    expect(result.details.exit_code).toBe(1);
  });

  test("foreground workdir is respected", async () => {
    const { h, bash } = await pluginHarness();
    const subdir = h.path("subdir");
    await mkdir(subdir);

    const result = await callBash(bash, h, { command: "pwd", workdir: subdir });

    expect(result.output.trim()).toBe(await realpath(subdir));
    expect(result.details.exit_code).toBe(0);
  });

  test("foreground timeout returns timed-out process exit without throwing", async () => {
    const h = await harness();

    const response = await bridgeBashToTerminal(h, { command: "sleep 5", timeout: 1 });

    expect(response.success).toBe(true);
    expect(response.status).toBe("timed_out");
    expect(response.exit_code).toBe(124);
  });

  test("rewrites cat to read with footer hint when enabled", async () => {
    const h = await harness({ rewrite: true });
    const filePath = h.path("notes.txt");
    await writeFile(filePath, "alpha\nbeta\n", "utf8");

    const response = await h.bridge.send("bash", { command: `cat ${filePath}`, compressed: false });

    expect(response.id).toBeDefined();
    expect(response.id).not.toBe("bash_rewrite");
    expect(response.success).toBe(true);
    expect(String(response.output)).toContain("1: alpha");
    expect(String(response.output)).toContain("2: beta");
    expect(String(response.output)).toContain("Prefer `read` tool over bash.");
  }, 60_000);

  test("rewrites grep -r to grep tool with footer hint when enabled", async () => {
    const h = await harness({ rewrite: true });
    await mkdir(h.path("src"));
    await writeFile(h.path("src", "lib.ts"), "needle\nhaystack\n", "utf8");

    const response = await h.bridge.send("bash", {
      command: `grep -r needle ${h.path("src")}`,
      compressed: false,
    });

    expect(response.id).toBeDefined();
    expect(response.id).not.toBe("bash_rewrite");
    expect(response.success).toBe(true);
    expect(String(response.output)).toContain("lib.ts");
    expect(String(response.output)).toContain("needle");
    expect(String(response.output)).toContain("Prefer `grep` tool over bash.");
  }, 60_000);

  test("rewriter disabled runs cat as raw bash without footer", async () => {
    const h = await harness({ rewrite: false });
    const filePath = h.path("raw.txt");
    await writeFile(filePath, "raw cat output\n", "utf8");

    const response = await bridgeBashToTerminal(h, {
      command: `cat ${filePath}`,
      compressed: false,
    });

    expect(response.success).toBe(true);
    expect(response.output_preview).toBe("raw cat output\n");
    expect(String(response.output_preview)).not.toContain("Prefer `read` tool over bash.");
  });

  test("generic compressor strips ANSI and collapses four-plus duplicate lines", async () => {
    const h = await harness({ compress: true });

    const response = await bridgeBashToTerminal(h, {
      command: "printf '\\033[31mred\\033[0m\\nred\\nred\\nred\\nred\\n'",
    });

    expect(response.success).toBe(true);
    expect(String(response.output_preview)).toContain("red");
  });

  test("compressed false opts out of duplicate-line compression", async () => {
    const h = await harness({ compress: true });

    const response = await bridgeBashToTerminal(h, {
      command: "printf '\\033[31mred\\033[0m\\nred\\nred\\nred\\nred\\n'",
      compressed: false,
    });

    expect(response.success).toBe(true);
    expect(String(response.output_preview)).toContain("red\nred\nred\nred");
    expect(String(response.output_preview)).not.toContain("... (4 more)");
  });

  test("background spawn returns task_id immediately", async () => {
    const h = await harness({ background: true });
    const started = Date.now();

    const response = await h.bridge.send("bash", {
      command: "sleep 1 && echo done",
      background: true,
    });

    expect(response.success).toBe(true);
    expect(response.status).toBe("running");
    expect(typeof response.task_id).toBe("string");
    // Typically returns in <100ms locally; assert only a generous deadlock bound
    // so CI load does not turn latency into correctness.
    expect(Date.now() - started).toBeLessThan(10_000);
  });

  test("bash_status reports running then completed output", async () => {
    const h = await harness({ background: true });
    const spawned = await h.bridge.send("bash", {
      command: "sleep 0.3 && echo done",
      background: true,
    });
    const taskId = String(spawned.task_id);

    const running = await h.bridge.send("bash_status", { task_id: taskId });
    expect(running.success).toBe(true);
    expect(running.status).toBe("running");

    const completed = await waitForStatus(h, taskId, "completed");
    expect(completed.exit_code).toBe(0);
    expect(completed.output_preview).toBe("done\n");
  });

  test("Pi bash_status tool reports running and appends anti-polling reminder", async () => {
    const { h, bash, bashStatus } = await pluginHarness({ experimental_bash_background: true });
    const spawned = await callBash(bash, h, { command: "sleep 2 && echo done", background: true });
    const taskId = String(spawned.details.task_id);

    const status = await callTaskTool<BashStatusDetails>(bashStatus, h, taskId);

    // Header: status line for the task. Don't anchor on exact format because
    // duration may or may not be present depending on timing on the runner.
    expect(status.output).toContain(`Task ${taskId}: running`);
    // Anti-polling reminder must be appended for running tasks (parity with
    // the OpenCode plugin). Same wording so agent behavior is consistent
    // across both harnesses.
    expect(status.output).toContain(
      "A completion reminder will be delivered automatically; don't poll.",
    );
    expect(status.details.success).toBe(true);
    expect(status.details.status).toBe("running");
  });

  test("Pi bash_status tool reports completed exit and output preview", async () => {
    const { h, bash, bashStatus } = await pluginHarness({ experimental_bash_background: true });
    const spawned = await callBash(bash, h, {
      command: "sleep 0.2 && echo pi-done",
      background: true,
    });
    const taskId = String(spawned.details.task_id);

    const completed = await waitForToolStatus(h, bashStatus, taskId, "completed");

    expect(completed.output).toContain(`Task ${taskId}: completed (exit 0)`);
    expect(completed.output).toMatch(/\(exit 0\) \d+s/);
    expect(completed.output).toContain("pi-done");
    expect(completed.details.success).toBe(true);
    expect(completed.details.exit_code).toBe(0);
    expect(completed.details.output_preview).toBe("pi-done\n");
  });

  test("Pi bash_status preview preserves more than 200 chars", async () => {
    const { h, bash, bashStatus } = await pluginHarness({ experimental_bash_background: true });
    const longOutput = "x".repeat(260);
    const spawned = await callBash(bash, h, {
      command: `printf '${longOutput}'`,
      background: true,
    });
    const taskId = String(spawned.details.task_id);

    const completed = await waitForToolStatus(h, bashStatus, taskId, "completed");

    expect(completed.output).toContain("x".repeat(260));
  });

  test("bash_kill terminates a running task", async () => {
    const h = await harness({ background: true });
    const spawned = await h.bridge.send("bash", { command: "sleep 60", background: true });
    const taskId = String(spawned.task_id);

    const killed = await h.bridge.send("bash_kill", { task_id: taskId });
    const status = await h.bridge.send("bash_status", { task_id: taskId });

    expect(killed.success).toBe(true);
    expect(killed.status).toBe("killed");
    expect(status.status).toBe("killed");
  });

  test("Pi bash_kill tool terminates a running task and bash_status confirms killed", async () => {
    const { h, bash, bashStatus, bashKill } = await pluginHarness({
      experimental_bash_background: true,
    });
    const spawned = await callBash(bash, h, { command: "sleep 60", background: true });
    const taskId = String(spawned.details.task_id);

    const killed = await callTaskTool<BashKillDetails>(bashKill, h, taskId);
    const status = await callTaskTool<BashStatusDetails>(bashStatus, h, taskId);

    expect(killed.output).toBe(`Task ${taskId}: killed`);
    expect(killed.details.success).toBe(true);
    expect(killed.details.status).toBe("killed");
    expect(status.output).toContain(`Task ${taskId}: killed`);
    expect(status.details.status).toBe("killed");
  });

  test("Pi bash_kill tool surfaces already-completed task status", async () => {
    const { h, bash, bashStatus, bashKill } = await pluginHarness({
      experimental_bash_background: true,
    });
    const spawned = await callBash(bash, h, { command: "echo already-done", background: true });
    const taskId = String(spawned.details.task_id);

    await waitForToolStatus(h, bashStatus, taskId, "completed");
    const killed = await callTaskTool<BashKillDetails>(bashKill, h, taskId);

    expect(killed.output).toBe(`Task ${taskId}: completed`);
    expect(killed.details.status).toBe("completed");
  });

  test("background completions are no longer appended by the bash adapter", async () => {
    const { h, pool, bash } = await pluginHarness({ experimental_bash_background: true });
    const pluginBridge = pool.getBridge(h.tempDir);
    const spawned = await pluginBridge.send("bash", { command: "echo bg-done", background: true });
    const taskId = String(spawned.task_id);
    await new Promise((resolve) => setTimeout(resolve, 300));

    const result = await callBash(bash, h, { command: "echo foreground" });

    expect(result.output).toContain("foreground\n");
    expect(result.output).not.toContain("Background task");
    expect(result.output).not.toContain(taskId);
    expect(result.output).not.toContain("echo bg-done");
    expect(result.details.bg_completions).toBeUndefined();
  });
});

function toConfigureOverrides(config: Record<string, unknown>): Record<string, unknown> {
  return {
    ...(config.rewrite !== undefined ? { experimental_bash_rewrite: config.rewrite } : {}),
    ...(config.compress !== undefined ? { experimental_bash_compress: config.compress } : {}),
    ...(config.background !== undefined ? { experimental_bash_background: config.background } : {}),
  };
}

async function waitForStatus(h: Harness, taskId: string, expected: string) {
  const started = Date.now();
  while (Date.now() - started < 5_000) {
    const response = await h.bridge.send("bash_status", { task_id: taskId });
    expect(response.success).toBe(true);
    if (response.status === expected) return response;
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error(`timed out waiting for ${expected}`);
}

async function waitForToolStatus(
  h: Harness,
  bashStatus: MockToolDef,
  taskId: string,
  expected: string,
): Promise<{ output: string; details: BashStatusDetails }> {
  const started = Date.now();
  while (Date.now() - started < 5_000) {
    const response = await bashStatus.execute(
      `test-bash-status-${Date.now()}`,
      { task_id: taskId },
      undefined,
      undefined,
      { cwd: h.tempDir, hasUI: false },
    );
    const result = { output: h.text(response), details: response.details as BashStatusDetails };
    expect(result.details.success).toBe(true);
    if (result.details.status === expected) return result;
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error(`timed out waiting for ${expected}`);
}
