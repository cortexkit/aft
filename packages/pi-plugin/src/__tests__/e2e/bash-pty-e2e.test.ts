/// <reference path="../../bun-test.d.ts" />
import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import { join } from "node:path";
import { BridgePool } from "@cortexkit/aft-bridge";
import { $ } from "bun";
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
const python = (
  await $`command -v python3 || command -v python`
    .quiet()
    .text()
    .catch(() => "")
).trim();

maybeDescribe("e2e bash PTY (Pi adapter + bridge + Rust)", () => {
  let harnesses: Harness[] = [];

  beforeAll(async () => {
    await prepareBinary();
  });

  afterEach(async () => {
    await Promise.allSettled(harnesses.map((harness) => harness.cleanup()));
    harnesses = [];
  });

  async function pluginHarness() {
    const h = await createHarness(initialBinary, {
      fixtureNames: [],
      config: { indexes: { trigram: false, semantic: false } },
      timeoutMs: 60_000,
    });
    harnesses.push(h);
    const pool = new BridgePool(
      h.binaryPath,
      { timeoutMs: 60_000 },
      configureParamsFromLegacyOverrides({
        project_root: h.tempDir,
        restrict_to_project_root: false,
        storage_dir: join(h.tempDir, ".aft-storage"),
        harness: "pi",
        experimental_bash_background: true,
      }),
    );
    const ctx: PluginContext = {
      pool,
      config: {} as PluginContext["config"],
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
      bash: tools.get("bash")!,
      status: tools.get("bash_status")!,
      write: tools.get("bash_write")!,
    };
  }

  function extCtx(h: Harness): MockExtensionContext {
    return { cwd: h.tempDir, hasUI: false };
  }

  async function call(
    tool: MockToolDef,
    h: Harness,
    params: Record<string, unknown>,
  ): Promise<string> {
    const result = await tool.execute(
      `test-${tool.name}-${Date.now()}`,
      params,
      undefined,
      undefined,
      extCtx(h),
    );
    return h.text(result);
  }

  async function waitForScreenText(
    status: MockToolDef,
    h: Harness,
    taskId: string,
    expected: readonly string[],
  ): Promise<string> {
    const started = Date.now();
    while (Date.now() - started < 5_000) {
      const out = await call(status, h, { task_id: taskId, output_mode: "screen" });
      if (expected.every((text) => out.includes(text))) return out;
      await sleep(100);
    }
    throw new Error(`timed out waiting for PTY screen to include ${expected.join(", ")}`);
  }

  function sleep(ms: number): Promise<void> {
    return new Promise((resolve) => setTimeout(resolve, ms));
  }

  test.skipIf(!python)("Test 28: pty_e2e_python_repl", async () => {
    const { h, bash, status, write } = await pluginHarness();
    const launched = await call(bash, h, {
      command: `${python} -q`,
      description: "Start a Python REPL",
      pty: true,
      background: true,
    });
    const taskId = launched.match(/bash-[a-zA-Z0-9_-]+/)?.[0];
    expect(taskId).toBeDefined();
    await call(write, h, { task_id: taskId, input: "print('hello')\n" });
    const out = await waitForScreenText(status, h, taskId!, ["hello"]);
    expect(out).toContain("hello");
    await call(write, h, { task_id: taskId, input: "exit()\n" });
  });

  test("Test 29: pty_e2e_ansi_screen_rendering", async () => {
    const { h, bash, status } = await pluginHarness();
    const command = `printf '\\033[2J\\033[Halpha\\033[10;5Hbeta'`;
    const launched = await call(bash, h, {
      command,
      description: "Render ANSI text on the PTY",
      pty: true,
      background: true,
    });
    const taskId = launched.match(/bash-[a-zA-Z0-9_-]+/)?.[0];
    expect(taskId).toBeDefined();
    const out = await waitForScreenText(status, h, taskId!, ["alpha", "beta"]);
    expect(out).toContain("alpha");
    expect(out).toContain("beta");
  });
});
