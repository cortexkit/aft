/// <reference path="../../bun-test.d.ts" />

import { afterEach, beforeAll, describe, expect, mock, test } from "bun:test";
import { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";

// Mock the live-server SDK factory + wake-availability decision so the
// wake path can route promptAsync to a test stub. The real implementation
// builds a `createOpencodeClient` pointed at `input.serverUrl`, which is
// not available in this real-bridge e2e harness (no OpenCode HTTP server
// fixture).
//
// Post-v0.29, when `useLiveServerWake()` returns false, the wake path
// falls back to `drainContext.client.session.promptAsync`. We pin it to
// `true` here so this e2e keeps exercising the workaround path; a
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
  __resetLiveServerClientCacheForTests: () => {
    e2eLiveServerClient = null;
  },
  __resetLiveServerWakeForTests: () => {},
}));

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

  test("turn-end wake sends promptAsync through OpenCode client", async () => {
    const { h, ctx, bash } = await pluginHarness();
    const taskId = await spawnBackground(h, bash, "printf idle-done");
    const promptCalls: unknown[] = [];
    // Install a stub live-server client that captures the wake POST. The
    // workaround intentionally bypasses `input.client` and would otherwise
    // try to reach `serverUrl` over HTTP — see anomalyco/opencode#28202.
    setE2ELiveServerClient({
      session: {
        promptAsync: async (payload: unknown) => {
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
