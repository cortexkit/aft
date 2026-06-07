/// <reference path="../../bun-test.d.ts" />

import { afterAll, afterEach, beforeAll, describe, expect, mock, test } from "bun:test";
import { BridgePool } from "../../../../aft-bridge/src/index.js";
import type { ToolContext } from "@opencode-ai/plugin";

// Mock live-server SDK factory + wake-availability decision so wake path can
// route sync `prompt` to test stub. Real implementation
// builds a `createOpencodeClient` pointed at `input.serverUrl`, which is
// not available in this real-bridge e2e harness (no OpenCode HTTP server
// fixture).
//
// When `useLiveServerWake()` returns false, wake path falls back to
// `drainContext.client.session.prompt(...)` or degraded `.promptAsync`.
// We pin it to `true` here so this e2e keeps exercising live wake path; a
// dedicated unit test covers the fallback branch in
// `__tests__/bg-notifications.test.ts`.
let e2eLiveServerClient: unknown = null;
function setE2ELiveServerClient(client: unknown): void {
  e2eLiveServerClient = client;
}
mock.module("../../shared/live-server-client.js", () => ({
  getLiveServerClient: () => {
    if (!e2eLiveServerClient) {
      throw new Error("e2e test did not configure a live-server client");
    }
    return e2eLiveServerClient;
  },
  useLiveServerWake: () => true,
  setLiveServerWakeAvailable: () => {},
  // Bun's `mock.module()` is process-global and partial mocks leak across
  // test files; the live-server-client unit tests import from this same
  // path, so probe-related exports MUST be included even if this file
  // doesn't exercise them.
  probeServerReachable: async () => true,
  __resetLiveServerClientCacheForTests: () => {
    e2eLiveServerClient = null;
  },
  __resetLiveServerWakeForTests: () => {},
}));

afterAll(() => {
  mock.restore();
});

import {
  __resetBgNotificationStateForTests,
  appendInTurnBgCompletions,
  handleIdleBgCompletions,
  handlePushedBgCompletion,
  handlePushedBgLongRunning,
  sessionBgStates,
  trackBgTask,
} from "../../bg-notifications.js";
import { createBashTool } from "../../tools/bash.js";
import type { PluginContext } from "../../types.js";
import { noopAsk } from "../test-helpers";
import {
  cleanupHarnesses,
  createHarness,
  type E2EHarness,
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

  async function pluginHarness() {
    const h = await createHarness(preparedBinary, {
      fixtureNames: [],
      bridgeOptions: { timeoutMs: 20_000 },
    });
    harnesses.push(h);
    const pool = new BridgePool(
      h.binaryPath,
      { timeoutMs: 20_000 },
      {
        project_root: h.tempDir,
        restrict_to_project_root: false,
        bash_permissions: false,
        experimental_bash_background: true,
        storage_dir: h.path(".aft-storage"),
        harness: "opencode",
      },
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

  test("turn-end wake sends prompt through OpenCode client", async () => {
    const { h, ctx, bash } = await pluginHarness();
    const taskId = await spawnBackground(h, bash, "printf idle-done");
    const promptCalls: unknown[] = [];
    // Install a stub live-server client that captures the wake POST. The
    // workaround intentionally bypasses `input.client` and would otherwise
    // try to reach `serverUrl` over HTTP — see anomalyco/opencode#28202.
    setE2ELiveServerClient({
      session: {
        prompt: async (payload: unknown) => {
          promptCalls.push(payload);
        },
        messages: async () => ({ data: [] }),
      },
    });

    await waitUntil(async () => {
      await handleIdleBgCompletions({
        ctx,
        directory: h.tempDir,
        sessionID: "e2e-session",
        client: {},
        serverUrl: "http://127.0.0.1:0/",
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

  test("turn-end urgent failure wake uses failed reminder class", async () => {
    const { h, ctx, bash } = await pluginHarness();
    const taskId = await spawnBackground(h, bash, "sh -c 'printf boom >&2; exit 1'");
    const promptCalls: unknown[] = [];
    setE2ELiveServerClient({
      session: {
        prompt: async (payload: unknown) => {
          promptCalls.push(payload);
        },
        messages: async () => ({ data: [] }),
      },
    });

    await waitUntil(async () => {
      await handleIdleBgCompletions({
        ctx,
        directory: h.tempDir,
        sessionID: "e2e-session",
        client: {},
        serverUrl: "http://127.0.0.1:0/",
      });
      return promptCalls.length > 0 || hasScheduledBgWake();
    });
    await waitUntil(() => promptCalls.length > 0, 5_000);

    const text = (promptCalls[0] as { body: { parts: Array<{ text: string }> } }).body.parts[0]
      .text;
    expect(text).toContain("[BACKGROUND BASH FAILED]");
    expect(text).toContain(`- task ${taskId} (exit 1)`);
    expect(text).toContain("boom");
  });

  test("autonomous long-running wake followed by completion wake fires twice", async () => {
    const { h, ctx, bash } = await pluginHarness();
    const taskId = await spawnBackground(h, bash, "sh -c 'sleep 0.2; printf done'");
    const promptCalls: unknown[] = [];
    setE2ELiveServerClient({
      session: {
        prompt: async (payload: unknown) => {
          promptCalls.push(payload);
        },
        messages: async () => ({ data: [] }),
      },
    });

    await handlePushedBgLongRunning(
      {
        ctx,
        directory: h.tempDir,
        sessionID: "e2e-session",
        client: {},
        serverUrl: "http://127.0.0.1:0/",
      },
      {
        task_id: taskId,
        session_id: "e2e-session",
        command: "sleep 0.2; printf done",
        elapsed_ms: 30_000,
      },
    );
    await waitUntil(() => promptCalls.length >= 1, 5_000);

    const firstText = (promptCalls[0] as { body: { parts: Array<{ text: string }> } }).body.parts[0]
      .text;
    expect(firstText).toContain("[BACKGROUND BASH STILL RUNNING]");
    expect(firstText).not.toContain("[BACKGROUND BASH COMPLETED]");
    expect(sessionBgStates.get("e2e-session")?.wakeDeferredTaskIds.has(taskId)).toBe(false);

    const bridge = ctx.pool.getActiveBridgeForRoot(h.tempDir) ?? ctx.pool.getBridge(h.tempDir);
    let pending: unknown;
    await waitUntil(async () => {
      const response = await bridge.send("bash_drain_completions", { session_id: "e2e-session" });
      const completions = Array.isArray((response as { bg_completions?: unknown[] }).bg_completions)
        ? (response as { bg_completions: unknown[] }).bg_completions
        : [];
      pending = completions.find(
        (completion) =>
          !!completion &&
          typeof completion === "object" &&
          (completion as { task_id?: unknown }).task_id === taskId,
      );
      return pending !== undefined;
    }, 5_000);

    expect(pending).toBeDefined();

    await handlePushedBgCompletion(
      {
        ctx,
        directory: h.tempDir,
        sessionID: "e2e-session",
        client: {},
        serverUrl: "http://127.0.0.1:0/",
      },
      pending,
    );
    await waitUntil(() => promptCalls.length >= 2, 5_000);

    const secondText = (promptCalls[1] as { body: { parts: Array<{ text: string }> } }).body
      .parts[0].text;
    expect(secondText).toContain("[BACKGROUND BASH COMPLETED]");
    expect(secondText).not.toContain("[BACKGROUND BASH STILL RUNNING]");
    expect(secondText).toContain(`- task ${taskId} (exit 0)`);
  });
});

async function spawnBackground(
  h: E2EHarness,
  bash: ReturnType<typeof createBashTool>,
  command: string,
): Promise<string> {
  const output = await bash.execute({ command, background: true }, {
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
