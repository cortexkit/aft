/// <reference path="../../bun-test.d.ts" />

import { afterAll, afterEach, beforeAll, describe, expect, test } from "bun:test";
import { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";

import {
  __resetBgNotificationStateForTests,
  appendInTurnBgCompletions,
  handleIdleBgCompletions,
  sessionBgStates,
  trackBgTask,
} from "../../bg-notifications.js";
import { createBashTool } from "../../tools/bash.js";
import type { PluginContext } from "../../types.js";
import { noopAsk } from "../test-helpers";
import {
  cleanupHarnesses,
  cleanupSharedSubcRig,
  configureParamsFromLegacyOverrides,
  createHarness,
  type E2EHarness,
  harnessPool,
  type PreparedBinary,
  prepareBinary,
} from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = describe.skipIf(!initialBinary.binaryPath);

maybeDescribe("e2e bg notifications (OpenCode adapter + bridge + Rust)", () => {
  let preparedBinary: PreparedBinary = initialBinary;
  const harnesses: E2EHarness[] = [];

  beforeAll(async () => {
    preparedBinary = await prepareBinary();
  });

  afterEach(async () => {
    __resetBgNotificationStateForTests();
    await cleanupHarnesses(harnesses);
  });

  // The subc tests below start the process-wide subc rig, whose daemon outlives
  // every harness in this file. Without this the daemon stays up for as long as
  // the machine does.
  afterAll(async () => {
    await cleanupSharedSubcRig();
  });

  async function pluginHarness() {
    const h = await createHarness(preparedBinary, {
      fixtureNames: [],
      bridgeOptions: { timeoutMs: 20_000 },
    });
    harnesses.push(h);
    const pool = new BridgePool(
      h.binaryPath,
      { timeoutMs: 20_000 },
      configureParamsFromLegacyOverrides({
        project_root: h.tempDir,
        restrict_to_project_root: false,
        bash_permissions: false,
        experimental_bash_background: true,
        storage_dir: h.path(".aft-storage"),
        harness: "opencode",
      }),
    );
    const ctx: PluginContext = {
      pool,
      client: {} as PluginContext["client"],
      config: {} as PluginContext["config"],
      storageDir: h.path(".aft-storage"),
    };
    const cleanup = h.cleanup;
    Object.defineProperty(h, "cleanup", {
      value: async () => {
        await pool.shutdown();
        await cleanup.call(h);
      },
    });
    return { h, ctx, bash: createBashTool(ctx) };
  }

  test("in-turn delivery appends reminder after another tool result", async () => {
    const { h, ctx, bash } = await pluginHarness();
    const taskId = await spawnBackground(h, bash, "printf done");
    const output = { output: "read output", title: "read", metadata: {} };

    await waitUntil(async () => {
      await appendInTurnBgCompletions(
        { ctx, directory: h.tempDir, sessionID: "e2e-session" },
        output,
      );
      return output.output.includes(taskId);
    });

    expect(output.output).toContain("<system-reminder>");
    expect(output.output).toContain(`- task ${taskId} (exit 0)`);
    // The new design ships output preview instead of the command, so the
    // captured `done` (printed by the bg task) should be present in the
    // indented preview block, while the command itself must NOT leak in.
    expect(output.output).toContain("    done");
    expect(output.output).not.toContain(": printf done");
  });

  async function subcPluginHarness() {
    const h = await createHarness(preparedBinary, {
      fixtureNames: [],
      transport: "subc",
      bridgeOptions: { timeoutMs: 20_000 },
      configOverrides: {
        restrict_to_project_root: false,
        bash_permissions: false,
        experimental_bash_background: true,
        bash: { background: true, foreground_wait_window_ms: 250 },
        harness: "opencode",
      },
    });
    harnesses.push(h);
    const rawBashResponses: Array<Record<string, unknown>> = [];
    const originalSend = h.bridge.send.bind(h.bridge);
    h.bridge.send = async (...args: Parameters<typeof h.bridge.send>) => {
      const result = await originalSend(...args);
      if (args[0] === "bash") rawBashResponses.push(result);
      return result;
    };
    const pool = harnessPool(h);
    const ctx: PluginContext = {
      pool,
      client: {} as PluginContext["client"],
      config: {} as PluginContext["config"],
      storageDir: h.path(".aft-storage"),
    };
    return { h, ctx, bash: createBashTool(ctx), pool, rawBashResponses };
  }

  test("detach response leaves an earlier completion queued for the next tool result", async () => {
    const { h, ctx, bash, pool, rawBashResponses } = await subcPluginHarness();
    const sessionID = "e2e-session";
    const bridge = pool.getBridge(h.tempDir);
    const alreadyFinishedTaskId = await spawnBackground(
      h,
      bash,
      process.platform === "win32"
        ? "Start-Sleep -Seconds 2; Write-Output older-finished"
        : "sleep 2; printf older-finished",
    );
    expect(sessionBgStates.get(sessionID)?.pendingCompletions ?? []).toHaveLength(0);
    await new Promise((resolve) => setTimeout(resolve, 2_500));
    const waitResult = bash.execute(
      {
        command:
          process.platform === "win32"
            ? "Start-Sleep -Seconds 40; Write-Output finished"
            : "sleep 40; printf finished",
        wait: true,
        timeout: 45_000,
      },
      {
        sessionID,
        messageID: "e2e-message",
        agent: "e2e-agent",
        directory: h.tempDir,
        worktree: h.tempDir,
        abort: new AbortController().signal,
        metadata: () => {},
        ask: noopAsk,
        callID: `call-${Date.now()}`,
      } as ToolContext,
    );

    await waitUntil(async () => {
      const response = await bridge.send("bash_wait_detach", { session_id: sessionID });
      return response.detached === true;
    });
    const detached = await waitResult;
    const detachedResult = detached as { output?: string; metadata?: { taskId?: string } };
    const taskId = detachedResult.metadata?.taskId;
    if (!taskId)
      throw new Error(`detached bash did not return a task id: ${detachedResult.output}`);

    expect(detachedResult.output).not.toContain(`- task ${alreadyFinishedTaskId} (exit 0`);
    expect(detachedResult.output).not.toContain(`- task ${taskId} (`);
    const detachedRaw = [...rawBashResponses].reverse().find((result) => result.task_id === taskId);
    if (!detachedRaw)
      throw new Error(
        `raw detach response missing for ${taskId}: ${JSON.stringify(rawBashResponses)}`,
      );
    const detachedSidecars = Array.isArray(detachedRaw.bg_completions)
      ? detachedRaw.bg_completions
      : [];
    expect(detachedSidecars).not.toContainEqual(
      expect.objectContaining({ task_id: alreadyFinishedTaskId }),
    );

    const beforeExit = [
      { output: "unrelated read result" },
      { output: "unrelated status result" },
      { output: "third unrelated tool result" },
    ];
    for (const output of beforeExit) {
      await appendInTurnBgCompletions({ ctx, directory: h.tempDir, sessionID }, output);
    }
    const beforeExitText = beforeExit.map((output) => output.output).join("\n");
    expect(beforeExitText).toContain(`- task ${alreadyFinishedTaskId} (exit 0`);
    expect(
      beforeExitText.match(new RegExp(`- task ${alreadyFinishedTaskId} \\(`, "g")),
    ).toHaveLength(1);
    expect(beforeExitText).not.toContain(`- task ${taskId} (`);
    const running = await bridge.send("bash_status", { session_id: sessionID, task_id: taskId });
    expect(running.status).toBe("running");
    const statusCompletions = Array.isArray(running.bg_completions) ? running.bg_completions : [];
    expect(statusCompletions).toHaveLength(0);

    const killed = await bridge.send("bash_kill", { session_id: sessionID, task_id: taskId });
    expect(killed.success).toBe(true);
    const afterExit: Array<{ output: string }> = [];
    await waitUntil(async () => {
      const output = { output: `post-exit tool ${afterExit.length + 1}` };
      afterExit.push(output);
      await appendInTurnBgCompletions({ ctx, directory: h.tempDir, sessionID }, output);
      return output.output.includes("[BACKGROUND BASH COMPLETED]");
    });
    for (const label of ["second post-exit tool", "third post-exit tool"]) {
      const output = { output: label };
      afterExit.push(output);
      await appendInTurnBgCompletions({ ctx, directory: h.tempDir, sessionID }, output);
    }

    const footerCount = afterExit.reduce(
      (count, output) =>
        count + (output.output.match(/\[BACKGROUND BASH COMPLETED\]/g)?.length ?? 0),
      0,
    );
    expect(footerCount).toBe(1);
    expect(afterExit.map((output) => output.output).join("\n")).toContain(`- task ${taskId} (`);
  }, 30_000);

  test("auto-promotion response leaves an earlier completion queued for the next tool result", async () => {
    const { h, ctx, bash, pool, rawBashResponses } = await subcPluginHarness();
    const sessionID = "e2e-session";
    const bridge = pool.getBridge(h.tempDir);
    const alreadyFinishedTaskId = await spawnBackground(
      h,
      bash,
      process.platform === "win32"
        ? "Start-Sleep -Seconds 2; Write-Output older-promote"
        : "sleep 2; printf older-promote",
    );
    expect(sessionBgStates.get(sessionID)?.pendingCompletions ?? []).toHaveLength(0);
    await new Promise((resolve) => setTimeout(resolve, 2_500));

    const promoted = (await bash.execute(
      {
        command:
          process.platform === "win32"
            ? "Start-Sleep -Seconds 40; Write-Output finished"
            : "sleep 40; printf finished",
        timeout: 45_000,
      },
      {
        sessionID,
        messageID: "e2e-promote-message",
        agent: "e2e-agent",
        directory: h.tempDir,
        worktree: h.tempDir,
        abort: new AbortController().signal,
        metadata: () => {},
        ask: noopAsk,
        callID: `call-${Date.now()}`,
      } as ToolContext,
    )) as { output?: string; metadata?: { taskId?: string } };
    const taskId = promoted.metadata?.taskId;
    if (!taskId) throw new Error(`promoted bash did not return a task id: ${promoted.output}`);

    expect(promoted.output).not.toContain(`- task ${alreadyFinishedTaskId} (exit 0`);
    const promotedRaw = [...rawBashResponses].reverse().find((result) => result.task_id === taskId);
    if (!promotedRaw) throw new Error(`raw promotion response missing for ${taskId}`);
    const promotedSidecars = Array.isArray(promotedRaw.bg_completions)
      ? promotedRaw.bg_completions
      : [];
    expect(promotedSidecars).not.toContainEqual(
      expect.objectContaining({ task_id: alreadyFinishedTaskId }),
    );

    const nextTool = { output: "tool result after promotion" };
    await appendInTurnBgCompletions({ ctx, directory: h.tempDir, sessionID }, nextTool);
    expect(nextTool.output).toContain(`- task ${alreadyFinishedTaskId} (exit 0`);
    expect(nextTool.output).not.toContain(`- task ${taskId} (`);

    const killed = await bridge.send("bash_kill", { session_id: sessionID, task_id: taskId });
    expect(killed.success).toBe(true);
    await waitUntil(async () => {
      const output = { output: "post-kill tool result" };
      await appendInTurnBgCompletions({ ctx, directory: h.tempDir, sessionID }, output);
      return output.output.includes(`- task ${taskId} (`);
    });
  }, 30_000);

  test("turn-end wake sends promptAsync through OpenCode client", async () => {
    const { h, ctx, bash } = await pluginHarness();
    const taskId = await spawnBackground(h, bash, "printf idle-done");
    const promptCalls: unknown[] = [];
    const client = {
      session: {
        promptAsync: async (payload: unknown) => {
          promptCalls.push(payload);
        },
        messages: async () => ({ data: [] }),
      },
    };

    await waitUntil(async () => {
      await handleIdleBgCompletions({
        ctx,
        directory: h.tempDir,
        sessionID: "e2e-session",
        client,
      });
      return promptCalls.length > 0 || hasScheduledBgWake();
    });
    await waitUntil(() => promptCalls.length > 0, 5_000);

    expect(promptCalls).toHaveLength(1);
    const text = (promptCalls[0] as { body: { parts: Array<{ text: string }> } }).body.parts[0]
      .text;
    expect(text).toContain(`- task ${taskId} (exit 0)`);
    expect(text).toContain("    idle-done");
    expect(text).not.toContain(": printf idle-done");
  });
});

async function spawnBackground(
  h: E2EHarness,
  bash: ReturnType<typeof createBashTool>,
  command: string,
): Promise<string> {
  const result = await bash.execute({ command, background: true }, {
    sessionID: "e2e-session",
    messageID: "e2e-message",
    agent: "e2e-agent",
    directory: h.tempDir,
    worktree: h.tempDir,
    abort: new AbortController().signal,
    metadata: () => {},
    ask: noopAsk,
    callID: `call-${Date.now()}`,
  } as ToolContext);
  // The bash tool returns `{ output, title, metadata }` (UI metadata on the
  // result); the agent-visible text is `output`.
  const output = typeof result === "string" ? result : (result?.output ?? "");
  // Spawn-line format: "Background task started: <taskId>. <anti-poll reminder>."
  // Match the taskId between the colon and the trailing period so the test
  // works regardless of any anti-poll text we append. taskId charset is
  // [a-zA-Z0-9_-] (Rust's bash_background::registry::generate_task_id).
  const match = String(output).match(/Background task started:\s+([\w-]+)/);
  if (!match) throw new Error(`could not extract taskId from output: ${output}`);
  const taskId = match[1];
  trackBgTask("e2e-session", taskId);
  return taskId;
}

async function waitUntil(
  predicate: () => boolean | Promise<boolean>,
  timeoutMs = 4_000,
): Promise<void> {
  const started = Date.now();
  while (!(await predicate())) {
    if (Date.now() - started > timeoutMs) throw new Error("timed out waiting for condition");
    await sleep(100);
  }
}

function hasScheduledBgWake(): boolean {
  return Array.from(sessionBgStates.values()).some(
    (state) => state.pendingCompletions.length > 0 || state.debounceTimer !== null,
  );
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
