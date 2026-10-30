/**
 * E2E coverage for Pi's hoisted bash tool.
 *
 * These tests intentionally exercise the real Rust binary through Pi's
 * BinaryBridge-backed adapter. Pi has no permission system, so the OpenCode
 * permission-scan e2e case is intentionally not mirrored here.
 */

/// <reference path="../../bun-test.d.ts" />

import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import { mkdir, realpath, utimes, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { type BinaryBridge, type BridgeOptions, BridgePool } from "@cortexkit/aft-bridge";
import { registerBashTool } from "../../tools/bash.js";
import type { PluginContext } from "../../types.js";
import {
  configureParamsFromLegacyOverrides,
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
      config: {
        indexes: { trigram: false, semantic: false },
        ...toConfigureOverrides(configOverrides),
      },
      timeoutMs: 60_000,
    });
    harnesses.push(created);
    return created;
  }

  async function pluginHarness(
    configOverrides: Record<string, unknown> = {},
    bridgeOptions: BridgeOptions = {},
  ) {
    const h = await harness();
    const pool = new BridgePool(
      h.binaryPath,
      { timeoutMs: 60_000, ...bridgeOptions },
      configureParamsFromLegacyOverrides({
        project_root: h.tempDir,
        restrict_to_project_root: false,
        storage_dir: join(h.tempDir, ".aft-storage"),
        harness: "pi",
        ...configOverrides,
      }),
    );
    const bridgeCalls: Array<{ command: string; params: Record<string, unknown> }> = [];
    const patchedBridges = new WeakSet<BinaryBridge>();
    const originalGetBridge = pool.getBridge.bind(pool);
    (pool as { getBridge: (projectRoot: string) => BinaryBridge }).getBridge = (projectRoot) => {
      const bridge = originalGetBridge(projectRoot);
      if (!patchedBridges.has(bridge)) {
        const originalSend = bridge.send.bind(bridge);
        bridge.send = async (command, params = {}, options) => {
          bridgeCalls.push({ command, params });
          return await originalSend(command, params, options);
        };
        patchedBridges.add(bridge);
      }
      return bridge;
    };
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
      bashWatch: tools.get("bash_watch")!,
      bridgeCalls,
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

  function nonConfigureCommands(
    calls: Array<{ command: string; params: Record<string, unknown> }>,
  ): string[] {
    return calls.filter((call) => call.command !== "configure").map((call) => call.command);
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
    const { h, bash, bridgeCalls } = await pluginHarness();

    const result = await callBash(bash, h, { command: "echo hello" });

    // The line after the output names the finished task.
    expect(result.output).toMatch(/^hello\n\[task bash-[0-9a-f]+\]$/);
    expect(result.details.exit_code).toBe(0);
    expect(nonConfigureCommands(bridgeCalls)).toEqual(["bash"]);
    expect(bridgeCalls[0].params).toMatchObject({
      foreground_orchestrate: true,
      block_to_completion: false,
    });
  });

  test("Pi hoisted bash renders CRLF output as text lines", async () => {
    const { h, bash } = await pluginHarness();

    const result = await callBash(bash, h, {
      command: "printf 'line1\\r\\nline2\\r\\nline3\\r\\n'",
      compressed: false,
    });

    expect(result.output).toMatch(/^line1\nline2\nline3\n\[task bash-[0-9a-f]+\]$/);
    expect(result.output).not.toContain("\r");
    expect(result.details.exit_code).toBe(0);
  });

  test("foreground non-zero exit is a successful tool response", async () => {
    const { h, bash, bridgeCalls } = await pluginHarness();

    const result = await callBash(bash, h, { command: "false" });

    expect(result.output).toMatch(/^\n\[exit code: 1\]\n\[task bash-[0-9a-f]+\]$/);
    expect(result.details.exit_code).toBe(1);
    expect(nonConfigureCommands(bridgeCalls)).toEqual(["bash"]);
  });

  test("foreground promotion returns server text without client poll/promote calls", async () => {
    const { h, bash, bridgeCalls } = await pluginHarness(
      { experimental_bash_background: true },
      // The engine reads AFT_TEST_FOREGROUND_WAIT_MS from its own process
      // environment, not from the bash request arguments.
      { childEnv: { AFT_TEST_FOREGROUND_WAIT_MS: "25" } },
    );

    // A terminal result wins over promotion even after the deadline. Keep the
    // command alive long enough for the engine to poll on a loaded runner.
    const result = await callBash(bash, h, { command: "sleep 5 && echo late" });

    expect(result.output).toContain("promoted to background");
    // Server text rounds 25 ms to 0s, unlike the default wait of several seconds.
    expect(result.output).toContain("didn't finish within 0s");
    expect(result.details.task_id).toMatch(/^bash-[a-f0-9]{16}$/);
    expect(nonConfigureCommands(bridgeCalls)).toEqual(["bash"]);
  }, 30_000);

  test("wait true returns a long foreground command directly", async () => {
    const { h, bash, bridgeCalls } = await pluginHarness(
      { experimental_bash_background: true },
      { childEnv: { AFT_TEST_FOREGROUND_WAIT_MS: "25" } },
    );

    const result = await callBash(bash, h, {
      command: "sleep 0.2 && echo waited",
      wait: true,
      timeout: 5_000,
    });

    expect(result.output).toContain("waited\n");
    expect(result.output).not.toContain("promoted to background");
    expect(nonConfigureCommands(bridgeCalls)).toEqual(["bash"]);
    expect(bridgeCalls[0].params).toMatchObject({
      wait: true,
      block_to_completion: true,
      timeout: 5_000,
    });
  }, 30_000);

  test("background true returns server launch text and task id without polling", async () => {
    const { h, bash, bridgeCalls } = await pluginHarness({ experimental_bash_background: true });

    const result = await callBash(bash, h, {
      command: "sleep 0.2 && echo background-done",
      background: true,
    });

    expect(result.output).toContain("Background task started:");
    expect(result.details.task_id).toMatch(/^bash-[a-f0-9]{16}$/);
    expect(nonConfigureCommands(bridgeCalls)).toEqual(["bash"]);
  });

  test("foreground workdir is respected", async () => {
    const { h, bash } = await pluginHarness();
    const subdir = h.path("subdir");
    await mkdir(subdir);

    const result = await callBash(bash, h, { command: "pwd", workdir: subdir });

    const [cwd, taskLine] = result.output.trim().split("\n");
    expect(cwd).toBe(await realpath(subdir));
    expect(taskLine).toMatch(/^\[task bash-[0-9a-f]+\]$/);
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
    const old = new Date(Date.now() - 61_000);
    await utimes(h.path("src"), old, old);

    const response = await h.bridge.send("bash", {
      command: `grep -r needle ${h.path("src")}`,
      compressed: false,
    });

    expect(response.id).toBeDefined();
    expect(response.id).not.toBe("bash_rewrite");
    expect(response.success).toBe(true);
    expect(String(response.output)).toContain("lib.ts");
    expect(String(response.output)).toContain("needle");
    // The default tool surface includes aft_search, so the footer directs code searches there.
    expect(String(response.output)).toContain("DO NOT search code by running grep/rg in bash");
    expect(String(response.output)).toContain("Use the `aft_search` tool instead");
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

    const completed = await waitForStatus(h.bridge, taskId, "completed");
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
    // Anti-polling reminder must be appended for running tasks. The e2e
    // harness runs headless (hasUI false), which Pi classifies as a worker
    // session: a worker is never woken by a completion reminder, so it is told
    // to wait with bash_watch instead.
    expect(status.output).toContain("To wait for it, call bash_watch; don't poll.");
    expect(status.output).not.toContain("completion reminder");
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
    await waitForStatus(pluginBridge, taskId, "completed");

    const result = await callBash(bash, h, { command: "echo foreground" });

    expect(result.output).toContain("foreground\n");
    expect(result.output).not.toContain("Background task");
    expect(result.output).not.toContain(taskId);
    expect(result.output).not.toContain("echo bg-done");
    expect(result.details.bg_completions).toBeUndefined();
  });

  // A sync watch must report the time it really held the call. The wall clock
  // is stepped forward 60 s shortly after the watch starts, as an NTP
  // correction or a wake from sleep can do. A watch timed with Date.now()
  // would end at that step and claim it waited 60 s; a monotonic watch keeps
  // its 2 s limit and says so. The harness has no UI, so Pi classifies it as a
  // worker session.
  test("bash_watch on a still-running task reports about 2 s elapsed and the timeout reason", async () => {
    const { h, bash, bashWatch, bashKill } = await pluginHarness({
      experimental_bash_background: true,
    });
    const spawned = await callBash(bash, h, { command: "sleep 30", background: true });
    const taskId = String(spawned.details.task_id);

    const realNow = Date.now;
    const stepAt = realNow() + 300;
    Date.now = () => {
      const now = realNow();
      return now >= stepAt ? now + 60_000 : now;
    };
    const started = performance.now();
    let result: Awaited<ReturnType<MockToolDef["execute"]>>;
    try {
      result = await bashWatch.execute(
        `test-bash-watch-${realNow()}`,
        { task_id: taskId, timeout_ms: 2_000 },
        undefined,
        undefined,
        { cwd: h.tempDir, hasUI: false },
      );
    } finally {
      Date.now = realNow;
    }
    const realElapsed = performance.now() - started;
    await callTaskTool<BashKillDetails>(bashKill, h, taskId);

    const text = h.text(result);
    const waited = (result.details as { waited?: { reason?: string; elapsed_ms?: number } }).waited;
    expect(waited?.reason).toBe("timeout");
    expect(realElapsed).toBeGreaterThanOrEqual(1_900);
    expect(realElapsed).toBeLessThan(10_000);
    const reported = Number(/Waited (\d+)ms \(limit 2000ms\)/.exec(text)?.[1]);
    expect(Math.abs(reported - realElapsed)).toBeLessThan(500);
    expect(waited?.elapsed_ms).toBe(reported);
    expect(text).toContain("timeout reached without match");
  }, 20_000);
});

function toConfigureOverrides(config: Record<string, unknown>): Record<string, unknown> {
  return {
    ...(config.rewrite !== undefined ? { experimental_bash_rewrite: config.rewrite } : {}),
    ...(config.compress !== undefined ? { experimental_bash_compress: config.compress } : {}),
    ...(config.background !== undefined ? { experimental_bash_background: config.background } : {}),
  };
}

async function waitForStatus(
  bridge: Pick<Harness["bridge"], "send">,
  taskId: string,
  expected: string,
) {
  const started = Date.now();
  while (Date.now() - started < 5_000) {
    const response = await bridge.send("bash_status", { task_id: taskId });
    expect(response.success).toBe(true);
    if (response.status === expected) return response;
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error(`timed out waiting for ${taskId} to reach ${expected}`);
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
