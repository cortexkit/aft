/// <reference path="../../bun-test.d.ts" />
import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import { join } from "node:path";
import { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { $ } from "bun";
import { createBashStatusTool, createBashTool } from "../../tools/bash.js";
import { createBashWriteTool } from "../../tools/bash_write.js";
import type { PluginContext } from "../../types.js";
import { noopAsk, toolResultText } from "../test-helpers";
import {
  cleanupHarnesses,
  configureParamsFromLegacyOverrides,
  createHarness,
  type E2EHarness,
  prepareBinary,
} from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = describe.skipIf(!initialBinary.binaryPath);
const python = (
  await $`command -v python3 || command -v python`
    .quiet()
    .text()
    .catch(() => "")
).trim();

maybeDescribe("e2e bash PTY (OpenCode adapter + bridge + Rust)", () => {
  const harnesses: E2EHarness[] = [];

  beforeAll(async () => {
    await prepareBinary();
  });

  afterEach(async () => {
    await cleanupHarnesses(harnesses);
  });

  async function pluginHarness() {
    const h = await createHarness(initialBinary, {
      fixtureNames: [],
      bridgeOptions: { timeoutMs: 20_000 },
    });
    harnesses.push(h);
    const pool = new BridgePool(
      h.binaryPath,
      { timeoutMs: 20_000 },
      configureParamsFromLegacyOverrides({
        restrict_to_project_root: false,
        storage_dir: join(h.tempDir, ".aft-storage"),
        harness: "opencode",
        experimental_bash_background: true,
      }),
    );
    const ctx: PluginContext = {
      pool,
      client: {} as PluginContext["client"],
      config: {} as PluginContext["config"],
      storageDir: join(h.tempDir, ".aft-storage"),
    };
    const cleanup = h.cleanup;
    Object.defineProperty(h, "cleanup", {
      value: async () => {
        await pool.shutdown();
        await cleanup.call(h);
      },
    });
    return {
      h,
      bash: createBashTool(ctx),
      status: createBashStatusTool(ctx),
      write: createBashWriteTool(ctx),
    };
  }

  function runtime(h: E2EHarness): ToolContext {
    return {
      sessionID: "pty-e2e-session",
      messageID: "message",
      agent: "agent",
      directory: h.tempDir,
      worktree: h.tempDir,
      abort: new AbortController().signal,
      metadata: () => {},
      ask: noopAsk,
    } as ToolContext;
  }

  async function screen(
    status: ReturnType<typeof createBashStatusTool>,
    h: E2EHarness,
    taskId: string,
  ): Promise<string> {
    return String(await status.execute({ taskId, outputMode: "screen" }, runtime(h)));
  }

  async function waitForScreenText(
    status: ReturnType<typeof createBashStatusTool>,
    h: E2EHarness,
    taskId: string,
    expected: string[],
    timeoutMs = 5_000,
  ): Promise<string> {
    const started = Date.now();
    let out = "";
    while (Date.now() - started < timeoutMs) {
      out = await screen(status, h, taskId);
      if (expected.every((text) => out.includes(text))) return out;
      await sleep(100);
    }
    throw new Error(
      `timed out waiting for PTY screen to contain ${expected.join(", ")}; last screen:
${out}`,
    );
  }

  function sleep(ms: number): Promise<void> {
    return new Promise((resolve) => setTimeout(resolve, ms));
  }

  test.skipIf(!python)("Test 28: pty_e2e_python_repl", async () => {
    const { h, bash, status, write } = await pluginHarness();
    const launched = toolResultText(
      await bash.execute(
        {
          command: `${python} -q`,
          description: "Start a Python REPL",
          pty: true,
          background: true,
        },
        runtime(h),
      ),
    );
    const taskId = launched.match(/bash-[a-zA-Z0-9_-]+/)?.[0];
    expect(taskId).toBeDefined();
    await write.execute({ taskId, input: "print('hello')\n" }, runtime(h));
    const out = await waitForScreenText(status, h, taskId!, ["hello"]);
    expect(out).toContain("hello");
    await write.execute({ taskId, input: "exit()\n" }, runtime(h));
  });

  test("Test 29: pty_e2e_ansi_screen_rendering", async () => {
    const { h, bash, status } = await pluginHarness();
    const command = `printf '\\033[2J\\033[Halpha\\033[10;5Hbeta'`;
    const launched = toolResultText(
      await bash.execute(
        { command, description: "Render ANSI text on the PTY", pty: true, background: true },
        runtime(h),
      ),
    );
    const taskId = launched.match(/bash-[a-zA-Z0-9_-]+/)?.[0];
    expect(taskId).toBeDefined();
    const out = await waitForScreenText(status, h, taskId!, ["alpha", "beta"]);
    expect(out).toContain("alpha");
    expect(out).toContain("beta");
  });
});
