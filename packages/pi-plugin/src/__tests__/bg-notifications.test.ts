/// <reference path="../bun-test.d.ts" />

import { afterAll, afterEach, beforeEach, describe, expect, mock, test } from "bun:test";

const sessionLogSpy = mock((_sessionID: string, _message: string, _data?: unknown) => {});
const sessionWarnSpy = mock((_sessionID: string, _message: string, _data?: unknown) => {});
mock.module("../logger.js", () => ({
  sessionLog: sessionLogSpy,
  sessionWarn: sessionWarnSpy,
  sessionError: () => {},
  log: () => {},
  warn: () => {},
  error: () => {},
  bridgeLogger: {
    log: () => {},
    warn: () => {},
    error: () => {},
    getLogFilePath: () => "",
  },
  getLogFilePath: () => "",
}));

afterAll(() => {
  mock.restore();
});

import {
  __resetBgNotificationStateForTests,
  __setBgNotificationHopTimeoutForTests,
  appendToolResultBgCompletions,
  cleanupIdleSessionStates,
  consumeBgCompletion,
  formatPatternMatchReminder,
  formatSystemReminder,
  handlePushedBgCompletion,
  handlePushedPatternMatch,
  handleSubcBgEventsNudge,
  handleTurnEndBgCompletions,
  ingestBgCompletions,
  markBgCompletionDelivered,
  markExplicitControl,
  markTaskWaiting,
  type PatternMatchEntry,
  SESSION_BG_STATE_IDLE_TTL_MS,
  sessionBgStates,
  trackBgTask,
} from "../bg-notifications.js";
import type { PluginContext } from "../types.js";

type BridgeResponse = Record<string, unknown>;

beforeEach(() => {
  sessionLogSpy.mockClear();
  sessionWarnSpy.mockClear();
});

afterEach(() => {
  __resetBgNotificationStateForTests();
});

describe("Pi background notifications", () => {
  test("PTY incomplete capture completion reminder preserves exit code and shows warning", () => {
    const reason = "PTY output may be incomplete: output capture failed before EOF";
    for (const exitCode of [0, 17]) {
      const text = formatSystemReminder([
        {
          task_id: "bash-incomplete",
          command: "deploy",
          status: exitCode === 0 ? "completed" : "failed",
          exit_code: exitCode,
          output_incomplete: true,
          status_reason: reason,
        },
      ]);
      expect(text).toContain(`task bash-incomplete (exit ${exitCode})`);
      expect(text).toContain(reason);
    }
  });

  test("failed memory-cap completion shows its reason without a capture warning", () => {
    const reason = "Command stopped: exceeded the 64 MiB memory cap (test enforcement).";
    expect(
      formatSystemReminder([
        {
          task_id: "bash-memory",
          command: "allocate",
          status: "failed",
          exit_code: 137,
          status_reason: reason,
        },
      ]),
    ).toContain(reason);
  });

  test("completion notice shows AFT's incomplete-listing sentence exactly", () => {
    const sentence =
      "AFT could not read every process in the task's group, so others may also be running.";
    const text = formatSystemReminder([
      {
        task_id: "bash-hidden",
        command: "deploy",
        status: "timed_out",
        exit_code: 124,
        live_descendants: null,
        live_descendants_unavailable: "listing_incomplete",
        live_descendants_summary: sentence,
      },
    ]);
    expect(text.split("\n")).toContain(`    ${sentence}`);
  });

  test("formats system reminder bullets with status and duration", () => {
    expect(
      formatSystemReminder([
        {
          task_id: "d2ed3a9e",
          status: "completed",
          exit_code: 0,
          command: "cargo test --release",
          duration_ms: 83_000,
        },
        {
          task_id: "4f5b71c2",
          status: "timed_out",
          exit_code: null,
          command: "npm install",
          duration_ms: 30_000,
        },
      ]),
    ).toBe(
      "<system-reminder>\n[BACKGROUND BASH COMPLETED]\n- task d2ed3a9e (exit 0, 1m 23s)\n- task 4f5b71c2 (timed out, 30s)\n</system-reminder>",
    );
  });

  test("uses Pi task_id syntax in truncated-output reminder", () => {
    expect(
      formatSystemReminder([
        {
          task_id: "task-1",
          status: "completed",
          exit_code: 0,
          command: "cmd",
          output_preview: "tail",
          output_truncated: true,
        },
      ]),
    ).toContain('bash_status({ task_id: "..." })');
  });

  test("markBgCompletionDelivered persists locally consumed completions", async () => {
    const send = mock(async () => ({ success: true, acked_task_ids: ["task-1"] }));
    const { ctx } = harness(send);

    await markBgCompletionDelivered({ ctx, directory: "/tmp/project", sessionID: "s1" }, "task-1");

    expect(send).toHaveBeenCalledWith("bash_ack_completions", {
      session_id: "s1",
      task_ids: ["task-1"],
    });
  });

  test("pending explicit control converts completions before task tracking", () => {
    markExplicitControl("s1", "task-1", false);

    const accepted = ingestBgCompletions("s1", [completion("task-1", "npm test")]);

    expect(accepted).toEqual([]);
    const state = sessionBgStates.get("s1");
    expect(state?.pendingCompletions).toHaveLength(0);
    expect(state?.pendingPatternMatches).toHaveLength(1);
    expect(state?.pendingPatternMatches[0]?.reason).toBe("task_exit");
  });

  test("emptying pending queues resets wake hard-stop retry state", () => {
    trackBgTask("s1", "task-1");
    ingestBgCompletions("s1", [completion("task-1", "npm test")]);
    const state = sessionBgStates.get("s1");
    expect(state?.pendingCompletions).toHaveLength(1);
    if (!state) throw new Error("missing state");
    state.retryDelayMs = 1000;
    state.wakeRetryAttempts = 5;
    state.wakeHardStopped = true;

    consumeBgCompletion("s1", "task-1");

    expect(state.pendingCompletions).toHaveLength(0);
    expect(state.retryDelayMs).toBeNull();
    expect(state.wakeRetryAttempts).toBe(0);
    expect(state.wakeHardStopped).toBe(false);
  });

  test("markExplicitControl retroactively converts already-pending completion to pattern match", () => {
    // Race: bash spawns → trackBgTask, completion push frame arrives →
    // ingestBgCompletions queues into pendingCompletions, THEN bash_watch
    // async runs markExplicitControl. Without retroactive conversion the
    // in-turn-append path would emit both "[BACKGROUND BASH COMPLETED]" and
    // "[BG BASH NOTIFY]" for the same task.
    trackBgTask("s1", "task-1");
    const accepted = ingestBgCompletions("s1", [completion("task-1", "sleep 3 && echo X")]);
    expect(accepted).toHaveLength(1);

    const stateBefore = sessionBgStates.get("s1");
    expect(stateBefore?.pendingCompletions).toHaveLength(1);
    expect(stateBefore?.pendingPatternMatches).toHaveLength(0);

    markExplicitControl("s1", "task-1", false);

    const stateAfter = sessionBgStates.get("s1");
    expect(stateAfter?.pendingCompletions).toHaveLength(0);
    expect(stateAfter?.pendingPatternMatches).toHaveLength(1);
    expect(stateAfter?.pendingPatternMatches[0]?.reason).toBe("task_exit");
    expect(stateAfter?.wakeDeferredTaskIds.has("task-1")).toBe(false);
  });

  test("retroactively converted task-exit notify is acked after tool-result delivery", async () => {
    trackBgTask("s1", "task-1");
    ingestBgCompletions("s1", [completion("task-1", "sleep 3 && echo X")]);
    markExplicitControl("s1", "task-1", false);
    const send = mock(async (command: string) =>
      command === "bash_ack_completions"
        ? { success: true, acked_task_ids: ["task-1"] }
        : { success: true, bg_completions: [] },
    );
    const { ctx } = harness(send);

    const content = await appendToolResultBgCompletions(
      { ctx, directory: "/tmp/project", sessionID: "s1" },
      [{ type: "text", text: "watch registered" }],
    );

    const reminder = content?.[1]?.type === "text" ? content[1].text : "";
    expect(reminder).toContain("[BG BASH NOTIFY]");
    expect(send).toHaveBeenCalledWith("bash_ack_completions", {
      session_id: "s1",
      task_ids: ["task-1"],
    });
  });

  test("late async watch renders one notify and suppresses default completion on drain", async () => {
    trackBgTask("s1", "task-1");
    const { ctx } = harness((command) =>
      command === "bash_drain_completions"
        ? { success: true, bg_completions: [completion("task-1", "echo READY")] }
        : { success: true, acked_task_ids: ["task-1"] },
    );
    const sendUserMessage = mock(() => {});

    await handlePushedBgCompletion(
      { ctx, directory: "/tmp/project", sessionID: "s1", runtime: { sendUserMessage } },
      completion("task-1", "echo READY"),
    );
    markExplicitControl("s1", "task-1", false);
    markExplicitControl("s1", "task-1");

    const content = await appendToolResultBgCompletions(
      { ctx, directory: "/tmp/project", sessionID: "s1" },
      [{ type: "text", text: "watch registered" }],
    );

    const reminder = content?.[1]?.type === "text" ? content[1].text : "";
    expect(reminder).toContain("[BG BASH NOTIFY]");
    expect(reminder).not.toContain("[BACKGROUND BASH COMPLETED]");
    expect(reminder.match(/- task task-1 exited:/g)).toHaveLength(1);
  });

  test("tool_result mutation appends a reminder text block", async () => {
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({
      success: true,
      bg_completions: [completion("task-1", "echo done")],
    }));

    const content = await appendToolResultBgCompletions(
      { ctx, directory: "/tmp/project", sessionID: "s1" },
      [{ type: "text", text: "tool output" }],
    );

    expect(content).toHaveLength(2);
    expect(content?.[1]).toEqual({
      type: "text",
      text: "<system-reminder>\n[BACKGROUND BASH COMPLETED]\n- task task-1 (exit 0)\n</system-reminder>",
    });
    expect(sessionBgStates.get("s1")?.pendingCompletions).toHaveLength(0);
  });

  test("first no-task path force-drains once for replayed completions", async () => {
    const send = mock(async () => ({ success: true, bg_completions: [] }));
    const { ctx } = harness(send);

    const content = await appendToolResultBgCompletions(
      { ctx, directory: "/tmp/project", sessionID: "s1" },
      [{ type: "text", text: "tool output" }],
    );

    expect(send).toHaveBeenCalledTimes(1);
    expect(send.mock.calls[0][0]).toBe("bash_drain_completions");
    expect(content).toBeUndefined();
  });

  test("idle hooks stay quiet without a bridge and replay once one appears", async () => {
    const send = mock(async (_command: string, _params: Record<string, unknown>) => ({
      success: true,
      bg_completions: [],
    }));
    const bridge = { send };
    let activeBridge: typeof bridge | null = null;
    const ctx = {
      pool: {
        getActiveBridgeForRoot: () => activeBridge,
        getBridge: () => activeBridge,
      },
      config: {},
      storageDir: "/tmp/aft-test",
    } as unknown as PluginContext;
    const drainContext = { ctx, directory: "/tmp/project", sessionID: "s1" };
    const runtime = { sendUserMessage: mock(() => {}) };

    await appendToolResultBgCompletions(drainContext, [{ type: "text", text: "first" }]);
    await appendToolResultBgCompletions(drainContext, [{ type: "text", text: "second" }]);
    await handleTurnEndBgCompletions({ ...drainContext, runtime });

    expect(send).toHaveBeenCalledTimes(0);
    expect(sessionWarnSpy).not.toHaveBeenCalled();
    expect(sessionBgStates.get("s1")?.forcedDrainCompleted).toBe(false);

    activeBridge = bridge;
    await appendToolResultBgCompletions(drainContext, [{ type: "text", text: "third" }]);
    await appendToolResultBgCompletions(drainContext, [{ type: "text", text: "fourth" }]);
    await handleTurnEndBgCompletions({ ...drainContext, runtime });

    expect(send.mock.calls.map((call) => call[0])).toEqual(["bash_drain_completions"]);
    expect(sessionWarnSpy).not.toHaveBeenCalled();
    expect(sessionBgStates.get("s1")?.forcedDrainCompleted).toBe(true);
  });

  test("tracked work still warns when its bridge is unavailable", async () => {
    const ctx = {
      pool: {
        getActiveBridgeForRoot: () => null,
        getBridge: () => null,
      },
      config: {},
      storageDir: "/tmp/aft-test",
    } as unknown as PluginContext;
    trackBgTask("s1", "task-1");

    await appendToolResultBgCompletions({ ctx, directory: "/tmp/project", sessionID: "s1" }, [
      { type: "text", text: "tool output" },
    ]);

    expect(
      sessionWarnSpy.mock.calls.some(
        (call) =>
          (call[2] as { event?: string } | undefined)?.event === "bash_completion_drain_error",
      ),
    ).toBe(true);
  });

  test("pending completions still warn when the bridge is unavailable", async () => {
    const ctx = {
      pool: {
        getActiveBridgeForRoot: () => null,
        getBridge: () => null,
      },
      config: {},
      storageDir: "/tmp/aft-test",
    } as unknown as PluginContext;
    trackBgTask("s1", "task-1");
    ingestBgCompletions("s1", [completion("task-1", "echo done")]);
    const runtime = { sendUserMessage: mock(() => {}) };

    await handleTurnEndBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      runtime,
    });

    expect(
      sessionWarnSpy.mock.calls.some(
        (call) =>
          (call[2] as { event?: string } | undefined)?.event === "bash_completion_drain_error",
      ),
    ).toBe(true);
  });

  test("completion replay uses a bounded non-killing transport poll", async () => {
    const send = mock(async (_command: string, _params: unknown, _options: unknown) => ({
      success: true,
      bg_completions: [],
    }));
    const ctx = {
      pool: { getActiveBridgeForRoot: () => ({ send }), getBridge: () => ({ send }) },
      config: {},
      storageDir: "/tmp/aft-test",
    } as unknown as PluginContext;
    await appendToolResultBgCompletions({ ctx, directory: "/tmp/project", sessionID: "s1" }, [
      { type: "text", text: "tool output" },
    ]);
    expect(send.mock.calls[0][0]).toBe("bash_drain_completions");
    expect(send.mock.calls[0][2]).toMatchObject({ timeoutMs: 15000, keepBridgeOnTimeout: true });
  });

  test("forced drain delivers replayed completion even when task is not tracked", async () => {
    const send = mock(async (command: string) =>
      command === "bash_drain_completions"
        ? { success: true, bg_completions: [completion("task-1", "echo replayed")] }
        : { success: true, acked_task_ids: ["task-1"] },
    );
    const { ctx } = harness(send);

    const content = await appendToolResultBgCompletions(
      { ctx, directory: "/tmp/project", sessionID: "s1" },
      [{ type: "text", text: "tool output" }],
    );

    expect(content?.[1]).toEqual({
      type: "text",
      text: "<system-reminder>\n[BACKGROUND BASH COMPLETED]\n- task task-1 (exit 0)\n</system-reminder>",
    });
    expect(send.mock.calls.map((call) => call[0])).toEqual([
      "bash_drain_completions",
      "bash_ack_completions",
    ]);
  });

  // Every tool result (AFT or not) runs this hook. With no task outstanding
  // and nothing pending, only the first result needs the replay drain; the
  // rest must not pay a bridge round trip each.
  test("idle tool results drain once per session, not once per result", async () => {
    const send = mock(async () => ({ success: true, bg_completions: [] }));
    const { ctx } = harness(send);

    for (let i = 0; i < 10; i++) {
      const content = await appendToolResultBgCompletions(
        { ctx, directory: "/tmp/project", sessionID: "s1" },
        [{ type: "text", text: `tool output ${i}` }],
      );
      expect(content).toBeUndefined();
    }

    expect(send.mock.calls.map((call) => call[0])).toEqual(["bash_drain_completions"]);
  });

  test("a task that finishes between two tool results is delivered on the second", async () => {
    let held: ReturnType<typeof completion>[] = [];
    const send = mock(async (command: string) =>
      command === "bash_drain_completions"
        ? { success: true, bg_completions: held }
        : { success: true, acked_task_ids: held.map((c) => c.task_id) },
    );
    const { ctx } = harness(send);
    const drainContext = { ctx, directory: "/tmp/project", sessionID: "s1" };

    expect(
      await appendToolResultBgCompletions(drainContext, [{ type: "text", text: "first" }]),
    ).toBeUndefined();

    // The bash tool spawns a background task, which then finishes with no
    // push frame reaching the plugin; the daemon only holds the completion.
    trackBgTask("s1", "task-1");
    held = [completion("task-1", "sleep 1")];

    const second = await appendToolResultBgCompletions(drainContext, [
      { type: "text", text: "second" },
    ]);
    expect(JSON.stringify(second)).toContain("- task task-1 (exit 0)");
  });

  test("a push for an untracked task re-arms the idle drain", async () => {
    let held: ReturnType<typeof completion>[] = [];
    const send = mock(async (command: string) =>
      command === "bash_drain_completions"
        ? { success: true, bg_completions: held }
        : { success: true, acked_task_ids: held.map((c) => c.task_id) },
    );
    const { ctx } = harness(send);
    const drainContext = { ctx, directory: "/tmp/project", sessionID: "s1" };

    await appendToolResultBgCompletions(drainContext, [{ type: "text", text: "first" }]);

    // A completion this process never tracked is buffered by the push
    // handler, not delivered; the daemon still holds it unacknowledged.
    held = [completion("task-9", "make")];
    await handlePushedBgCompletion(
      { ...drainContext, runtime: { sendUserMessage: mock(async () => {}) } },
      held[0],
    );

    const second = await appendToolResultBgCompletions(drainContext, [
      { type: "text", text: "second" },
    ]);
    expect(JSON.stringify(second)).toContain("- task task-9 (exit 0)");
  });

  test("a failed idle drain is retried on the next tool result", async () => {
    let fail = true;
    const send = mock(async () =>
      fail ? { success: false, message: "busy" } : { success: true, bg_completions: [] },
    );
    const { ctx } = harness(send);
    const drainContext = { ctx, directory: "/tmp/project", sessionID: "s1" };

    await appendToolResultBgCompletions(drainContext, [{ type: "text", text: "first" }]);
    fail = false;
    await appendToolResultBgCompletions(drainContext, [{ type: "text", text: "second" }]);
    await appendToolResultBgCompletions(drainContext, [{ type: "text", text: "third" }]);

    expect(send).toHaveBeenCalledTimes(2);
  });

  test("turn-end wake sends one runtime user message with reminder", async () => {
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({
      success: true,
      bg_completions: [completion("task-1", "npm test")],
    }));
    const sendUserMessage = mock(() => {});

    await handleTurnEndBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      runtime: { sendUserMessage },
    });
    await waitForMockCallCount(sendUserMessage, 1);

    expect(sendUserMessage).toHaveBeenCalledTimes(1);
    expect(sendUserMessage.mock.calls[0][0]).toContain("- task task-1 (exit 0)");
    expect(sendUserMessage.mock.calls[0][0]).not.toContain(": npm test");
    // Regression: Pi's sendUserMessage rejects with "Agent is already
    // processing" when the agent is mid-turn unless we pass `deliverAs`.
    // The wake path must always pass `followUp` so a turn that starts
    // between our isActive check and the debounced send still queues
    // cleanly instead of throwing.
    expect(sendUserMessage.mock.calls[0][1]).toEqual({ deliverAs: "steer" });
  });

  test("push completion lands in pending and wakes after the spawn turn is idle", async () => {
    trackBgTask("s1", "task-1");
    const send = mock(async () => ({
      success: true,
      bg_completions: [],
      acked_task_ids: ["task-1"],
    }));
    const { ctx } = harness(send);
    const sendUserMessage = mock(() => {});
    await handleTurnEndBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      runtime: { sendUserMessage },
    });

    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        runtime: { sendUserMessage },
      },
      completion("task-1", "npm test"),
    );
    await waitForMockCallCount(sendUserMessage, 1);

    expect(sendUserMessage).toHaveBeenCalledTimes(1);
    expect(sendUserMessage.mock.calls[0][0]).toContain("- task task-1 (exit 0)");
    expect(sendUserMessage.mock.calls[0][0]).not.toContain(": npm test");
    expect(sessionBgStates.get("s1")?.pendingCompletions).toHaveLength(0);
    expect(send.mock.calls.some((call) => call[0] === "bash_ack_completions")).toBe(true);
  });

  test("same-turn push completion waits for sync bash_watch instead of waking", async () => {
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({ success: true, bg_completions: [] }));
    const sendUserMessage = mock(() => {});

    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        runtime: { sendUserMessage },
      },
      completion("task-1", "npm test"),
    );

    // Same-turn completions are deferred synchronously: they remain pending,
    // but no wake timer is scheduled until a turn-end boundary clears the
    // deferral. No wall-clock sleep is needed to prove the negative path.
    expect(sendUserMessage).toHaveBeenCalledTimes(0);
    expect(sessionBgStates.get("s1")?.pendingCompletions).toHaveLength(1);
    expect(sessionBgStates.get("s1")?.debounceTimer).toBeNull();

    markTaskWaiting("s1", "task-1");

    // A synchronous bash_watch wait consumes the pending completion immediately.
    expect(sendUserMessage).toHaveBeenCalledTimes(0);
    expect(sessionBgStates.get("s1")?.pendingCompletions).toHaveLength(0);
  });

  test("buffers push completion received before task tracking", async () => {
    const { ctx } = harness(() => ({ success: true, bg_completions: [] }));
    const sendUserMessage = mock(() => {});

    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        runtime: { sendUserMessage },
      },
      completion("task-1", "npm test"),
    );
    trackBgTask("s1", "task-1");
    await handleTurnEndBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      runtime: { sendUserMessage },
    });
    await waitForMockCallCount(sendUserMessage, 1);

    expect(sendUserMessage).toHaveBeenCalledTimes(1);
    expect(sendUserMessage.mock.calls[0][0]).toContain("- task task-1 (exit 0)");
  });

  test("failed wake keeps pending completions and retries", async () => {
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({ success: true, bg_completions: [] }));
    const sendUserMessage = mock(() => {
      throw new Error("send failed");
    });
    await handleTurnEndBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      runtime: { sendUserMessage },
    });

    await handlePushedBgCompletion(
      { ctx, directory: "/tmp/project", sessionID: "s1", runtime: { sendUserMessage } },
      completion("task-1", "npm test"),
    );
    await waitForMockCallCount(sendUserMessage, 1);

    expect(sendUserMessage).toHaveBeenCalledTimes(1);
    expect(sessionBgStates.get("s1")?.pendingCompletions).toHaveLength(1);
    expect(sessionBgStates.get("s1")?.debounceTimer).not.toBeNull();
  });

  test("failed wake hard-stops after capped retries", async () => {
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({ success: true, bg_completions: [] }));
    const sendUserMessage = mock(() => {
      throw new Error("send failed");
    });
    await handleTurnEndBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      runtime: { sendUserMessage },
    });

    await handlePushedBgCompletion(
      { ctx, directory: "/tmp/project", sessionID: "s1", runtime: { sendUserMessage } },
      completion("task-1", "npm test"),
    );
    await waitUntil(
      () =>
        sendUserMessage.mock.calls.length >= 5 && sessionBgStates.get("s1")?.debounceTimer === null,
      10_000,
    );

    expect(sendUserMessage).toHaveBeenCalledTimes(5);
    expect(sessionBgStates.get("s1")?.pendingCompletions).toHaveLength(1);
    expect(sessionBgStates.get("s1")?.debounceTimer).toBeNull();
  });

  test("drain uses Rust's default session when Pi session id is unknown", async () => {
    trackBgTask(undefined, "task-1");
    const send = mock(async () => ({
      success: true,
      bg_completions: [completion("task-1", "cmd")],
    }));
    const { ctx } = harness(send);

    await appendToolResultBgCompletions({ ctx, directory: "/tmp/project", sessionID: undefined }, [
      { type: "text", text: "normal" },
    ]);

    expect(send).toHaveBeenCalledTimes(2);
    expect(send.mock.calls[0][0]).toBe("bash_drain_completions");
    expect(send.mock.calls[0][1]).toEqual({});
    expect(send.mock.calls[1][0]).toBe("bash_ack_completions");
    expect(send.mock.calls[1][1]).toEqual({ task_ids: ["task-1"] });
    expect(sessionBgStates.get("__default__")?.outstandingTaskIds.has("task-1")).toBe(false);
  });

  test("post-idle push completion still wakes even when bridge is busy with non-agent RPC", async () => {
    // Regression: previously bailed on `isActive()` (bridge.hasPendingRequests())
    // which returned true for the TUI status poll, orphaning the completion when
    // no other trigger fired. Once the spawn turn has gone idle, the wake must
    // still be scheduled.
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({ success: true, bg_completions: [] }));
    const sendUserMessage = mock(() => {});
    await handleTurnEndBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      runtime: { sendUserMessage },
    });

    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        runtime: { sendUserMessage },
      },
      completion("task-1", "npm test"),
    );
    await waitForMockCallCount(sendUserMessage, 1);

    expect(sendUserMessage).toHaveBeenCalledTimes(1);
    expect(sendUserMessage.mock.calls[0][0]).toContain("task-1");
    expect(sendUserMessage.mock.calls[0][1]).toEqual({ deliverAs: "steer" });
    expect(sessionBgStates.get("s1")?.pendingCompletions).toHaveLength(0);
  });

  test("coalesces three idle completions into one notification", async () => {
    const responses = [
      { success: true, bg_completions: [completion("task-1", "one")] },
      { success: true, bg_completions: [completion("task-2", "two")] },
      { success: true, bg_completions: [completion("task-3", "three")] },
    ];
    const { ctx } = harness(() => responses.shift() ?? { success: true, bg_completions: [] });
    const sendUserMessage = mock(() => {});

    for (const taskId of ["task-1", "task-2", "task-3"]) trackBgTask("s1", taskId);
    // Each turn-end drain synchronously queues one completion and extends the
    // same debounce timer. Driving the drains back-to-back keeps coverage of
    // coalescing without relying on short sleeps landing inside a timer window.
    await handleTurnEndBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      runtime: { sendUserMessage },
    });
    await handleTurnEndBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      runtime: { sendUserMessage },
    });
    await handleTurnEndBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      runtime: { sendUserMessage },
    });
    await waitForMockCallCount(sendUserMessage, 1);

    expect(sendUserMessage).toHaveBeenCalledTimes(1);
    expect(String(sendUserMessage.mock.calls[0][0]).match(/^- task/gm)).toHaveLength(3);
  });

  test("debounce cap forces wake before the ticking finishes", async () => {
    // Contract under test: when completions arrive faster than the
    // debounce step window, the cap (DEBOUNCE_CAP_MS = 1000ms in
    // bg-notifications.ts) must fire at least one wake before the ticking
    // would naturally settle. Previously this asserted "exactly 1 wake
    // within wall-clock 950-1400ms"; both bounds were brittle under load.
    // The behavior the cap exists to prevent is "infinite reset" — at
    // least one wake MUST happen during the tick window. That's what we
    // check now.
    let index = 0;
    const { ctx } = harness(() => ({
      success: true,
      bg_completions: [completion(`task-${++index}`, `cmd-${index}`)],
    }));
    const sendUserMessage = mock(() => {});
    const started = Date.now();

    for (let task = 1; task <= 6; task++) trackBgTask("s1", `task-${task}`);
    for (let tick = 0; tick < 6; tick++) {
      await handleTurnEndBgCompletions({
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        runtime: { sendUserMessage },
      });
      await sleep(190);
    }
    await sleep(120);

    // At least one wake fired during the tick sequence. Without the cap
    // every tick would reset the debounce timer and no wake would ever
    // fire until the final 120ms tail. Under load multiple wakes can
    // fire (cap + trailing ticks), which is fine — what matters is the
    // cap engaged at all.
    expect(sendUserMessage.mock.calls.length).toBeGreaterThanOrEqual(1);
    // Lower bound proves the cap actually delayed wakes past ~1s
    // instead of firing instantly on the first completion.
    expect(Date.now() - started).toBeGreaterThanOrEqual(950);
  });

  test("second background completion wakes without input reset", async () => {
    const sendUserMessage = mock(() => {});
    let responses: BridgeResponse[] = [
      { success: true, bg_completions: [completion("task-1", "one")] },
    ];
    const { ctx } = harness(() => responses.shift() ?? { success: true, bg_completions: [] });

    trackBgTask("s1", "task-1");
    await handleTurnEndBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      runtime: { sendUserMessage },
    });
    await waitForMockCallCount(sendUserMessage, 1);
    await handleTurnEndBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      runtime: { sendUserMessage },
    });
    expect(sessionBgStates.get("s1")?.debounceTimer ?? null).toBeNull();
    expect(sendUserMessage).toHaveBeenCalledTimes(1);

    responses = [{ success: true, bg_completions: [completion("task-2", "two")] }];
    trackBgTask("s1", "task-2");
    await handleTurnEndBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      runtime: { sendUserMessage },
    });
    await waitForMockCallCount(sendUserMessage, 2);
    expect(sendUserMessage).toHaveBeenCalledTimes(2);
  });

  test("multi-session state is isolated", async () => {
    const { ctx } = harness((_, params) => ({
      success: true,
      bg_completions: [
        completion(params.session_id === "s1" ? "task-1" : "task-2", String(params.session_id)),
      ],
    }));

    trackBgTask("s1", "task-1");
    trackBgTask("s2", "task-2");
    const s1 = await appendToolResultBgCompletions(
      { ctx, directory: "/tmp/project", sessionID: "s1" },
      [{ type: "text", text: "one" }],
    );

    expect(s1?.[1].type === "text" ? s1[1].text : "").toContain("task-1");
    expect(sessionBgStates.get("s2")?.outstandingTaskIds.has("task-2")).toBe(true);

    const s2 = await appendToolResultBgCompletions(
      { ctx, directory: "/tmp/project", sessionID: "s2" },
      [{ type: "text", text: "two" }],
    );
    expect(s2?.[1].type === "text" ? s2[1].text : "").toContain("task-2");
  });

  test("cleanupIdleSessionStates evicts stale task-free sessions", () => {
    trackBgTask("stale", "task-stale");
    trackBgTask("fresh", "task-fresh");
    ingestCompletionForCleanup("stale", "task-stale");
    ingestCompletionForCleanup("fresh", "task-fresh");

    const now = Date.now();
    const stale = sessionBgStates.get("stale");
    const fresh = sessionBgStates.get("fresh");
    expect(stale).toBeDefined();
    expect(fresh).toBeDefined();
    if (!stale || !fresh) throw new Error("expected test states to exist");
    stale.lastSeenAt = now - SESSION_BG_STATE_IDLE_TTL_MS - 1;
    fresh.lastSeenAt = now;

    cleanupIdleSessionStates(now);

    expect(sessionBgStates.has("stale")).toBe(false);
    expect(sessionBgStates.has("fresh")).toBe(true);
  });

  test("drain failure does not break tool_result mutation", async () => {
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => {
      throw new Error("bridge down");
    });

    const content = await appendToolResultBgCompletions(
      { ctx, directory: "/tmp/project", sessionID: "s1" },
      [{ type: "text", text: "normal" }],
    );

    expect(content).toBeUndefined();
  });
});

describe("Pi subc forced-drain dedup (C-#1 / C-#3)", () => {
  test("a pushed watch-only tombstone is acked under its row session after delivery", async () => {
    const bridgeCalls: Array<{ command: string; params: Record<string, unknown> }> = [];
    const send = mock(async (command: string, params: Record<string, unknown>) => {
      bridgeCalls.push({ command, params });
      return { success: true, acked_task_ids: ["task-erased"] };
    });
    const { ctx } = harness(send);
    const sendUserMessage = mock(() => {});

    await handlePushedPatternMatch(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "row-session",
        runtime: { sendUserMessage },
      },
      {
        task_id: "task-erased",
        session_id: "row-session",
        watch_id: "watch-erased",
        match_text: "watch target erased",
        match_offset: 0,
        context:
          "watch target erased: the background task row was erased before the watch reached a normal terminal result",
        once: true,
        reason: "task_exit",
      },
    );
    await waitForMockCallCount(sendUserMessage, 1);
    await sleep(50);

    expect(sendUserMessage.mock.calls[0]?.[0]).toContain("watch target erased");
    expect(bridgeCalls).toContainEqual({
      command: "bash_ack_completions",
      params: { session_id: "row-session", task_ids: ["task-erased"] },
    });
  });

  test("drops pushed pattern match frames for a foreign session before sendUserMessage", async () => {
    const bridgeCalls: Array<{ command: string; params: Record<string, unknown> }> = [];
    const send = mock(async (command: string, params: Record<string, unknown>) => {
      bridgeCalls.push({ command, params });
      return { success: true };
    });
    const { ctx } = harness(send);
    const sendUserMessage = mock(() => {});

    await handlePushedPatternMatch(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "live-session",
        runtime: { sendUserMessage },
      },
      {
        task_id: "task-foreign",
        session_id: "foreign-session",
        watch_id: "watch-foreign",
        match_text: "watch target erased",
        match_offset: 0,
        context:
          "watch target erased: the background task row was erased before the watch reached a normal terminal result",
        once: true,
        reason: "task_exit",
      },
    );

    await sleep(250);
    expect(sendUserMessage).not.toHaveBeenCalled();
    expect(sessionWarnSpy).toHaveBeenCalledWith(
      "live-session",
      expect.stringContaining("dropped frame for foreign session: foreign-session"),
      expect.objectContaining({
        dropped_session_id: "foreign-session",
      }),
    );
  });

  test("logPerTaskDeliveryHop logs pattern-match-only deliveries with empty task list", async () => {
    const bridgeCalls: Array<{ command: string; params: Record<string, unknown> }> = [];
    const send = mock(async (command: string, params: Record<string, unknown>) => {
      bridgeCalls.push({ command, params });
      return { success: true, acked_task_ids: ["task-pattern-only"] };
    });
    const { ctx } = harness(send);
    const sendUserMessage = mock(() => {});

    await handlePushedPatternMatch(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "session-live",
        runtime: { sendUserMessage },
      },
      {
        task_id: "task-pattern-only",
        session_id: "session-live",
        watch_id: "watch-pattern-only",
        match_text: "matched",
        match_offset: 0,
        context: "matched text",
        once: true,
        reason: "pattern_match",
      },
    );

    await waitForMockCallCount(sendUserMessage, 1);
    expect(sendUserMessage).toHaveBeenCalled();

    const loggedHop = sessionLogSpy.mock.calls.find(
      (call) =>
        typeof call[1] === "string" &&
        call[1].includes("session inject start") &&
        (call[2] as Record<string, unknown>)?.watch_id === "watch-pattern-only",
    );
    expect(loggedHop).toBeDefined();
  });

  test("an erased-bundle replay is acked after delivery and stays gone on reopen", async () => {
    let pending = true;
    const events: string[] = [];
    const bridgeCalls: Array<{ command: string; params: Record<string, unknown> }> = [];
    const send = mock(async (command: string, params: Record<string, unknown>) => {
      events.push(command);
      bridgeCalls.push({ command, params });
      if (command === "bash_drain_completions") {
        return {
          success: true,
          bg_completions: [],
          pending_matches: pending
            ? [
                {
                  task_id: "task-erased",
                  session_id: "s1",
                  watch_id: "watch-erased",
                  match_text: "watch target erased",
                  match_offset: 0,
                  context: "background task artifacts were erased",
                  once: true,
                  reason: "task_exit",
                },
              ]
            : [],
        };
      }
      if (command === "bash_ack_completions") pending = false;
      return { success: true, acked_task_ids: ["task-erased"] };
    });
    const { ctx } = harness(send);
    const firstSessionMessages = mock(() => events.push("deliver"));

    await handleTurnEndBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      runtime: { sendUserMessage: firstSessionMessages },
    });
    await waitForMockCallCount(firstSessionMessages, 1);
    await sleep(50);

    __resetBgNotificationStateForTests();
    const reopenedSessionMessages = mock(() => {});
    await handleTurnEndBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      runtime: { sendUserMessage: reopenedSessionMessages },
    });
    await sleep(500);

    expect(reopenedSessionMessages).toHaveBeenCalledTimes(0);
    const ackCalls = bridgeCalls.filter((call) => call.command === "bash_ack_completions");
    expect(ackCalls).toHaveLength(1);
    expect(ackCalls[0]?.params).toEqual({ session_id: "s1", task_ids: ["task-erased"] });
    expect(events.indexOf("deliver")).toBeLessThan(events.indexOf("bash_ack_completions"));
  });

  test("a forced drain delivers and acknowledges a durable pattern match", async () => {
    const send = mock(async (command: string) =>
      command === "bash_drain_completions"
        ? {
            success: true,
            bg_completions: [],
            pending_matches: [
              {
                task_id: "task-pattern",
                session_id: "s1",
                watch_id: "watch-1",
                match_text: "READY",
                match_offset: 42,
                context: "server READY",
                once: true,
                reason: "pattern_match",
              },
            ],
          }
        : {
            success: true,
            acked_task_ids: [],
            acked_matches: [{ task_id: "task-pattern", watch_id: "watch-1", match_offset: 42 }],
          },
    );
    const { ctx } = harness(send);
    const sendUserMessage = mock(() => {});

    await handleSubcBgEventsNudge({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      runtime: { sendUserMessage },
    });
    await waitForMockCallCount(sendUserMessage, 1);
    await waitUntil(
      () => send.mock.calls.filter((call) => call[0] === "bash_ack_completions").length === 1,
    );

    expect(sendUserMessage.mock.calls[0][0]).toContain('task task-pattern matched "READY"');
    expect(send.mock.calls.find((call) => call[0] === "bash_ack_completions")?.[1]).toEqual({
      session_id: "s1",
      matches: [{ task_id: "task-pattern", watch_id: "watch-1", match_offset: 42 }],
    });
  });

  test("a forced drain renders and acknowledges a durable task-exit match once", async () => {
    const send = mock(async (command: string) =>
      command === "bash_drain_completions"
        ? {
            success: true,
            bg_completions: [],
            pending_matches: [
              {
                task_id: "task-exit",
                session_id: "s1",
                watch_id: "exit",
                match_text: "watch task exited",
                match_offset: 0,
                context: "task task-exit exited (exit 0)\nserver stopped",
                once: true,
                reason: "task_exit",
              },
            ],
          }
        : { success: true, acked_task_ids: ["task-exit"] },
    );
    const { ctx } = harness(send);
    const sendUserMessage = mock(() => {});

    await handleSubcBgEventsNudge({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      runtime: { sendUserMessage },
    });
    await waitForMockCallCount(sendUserMessage, 1);
    await waitUntil(
      () => send.mock.calls.filter((call) => call[0] === "bash_ack_completions").length === 1,
    );

    expect(sendUserMessage.mock.calls[0][0]).toContain("- task task-exit exited:");
    expect(sendUserMessage.mock.calls[0][0]).toContain("task task-exit exited (exit 0)");
    expect(sendUserMessage.mock.calls[0][0]).not.toContain("matched");
    const ackCalls = send.mock.calls.filter((call) => call[0] === "bash_ack_completions");
    expect(ackCalls).toHaveLength(1);
    expect(ackCalls[0]?.[1]).toEqual({ session_id: "s1", task_ids: ["task-exit"] });
  });

  test("coalesces concurrent multi-record nudge fan-in at handler entry", async () => {
    let releaseDrain!: () => void;
    const drainGate = new Promise<void>((resolve) => {
      releaseDrain = resolve;
    });
    const send = mock(async (command: string) => {
      if (command === "bash_drain_completions") await drainGate;
      return { success: true, bg_completions: [] };
    });
    const { ctx } = harness(send);
    const drainCtx = {
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      runtime: { sendUserMessage: mock(() => {}) },
    };

    const first = handleSubcBgEventsNudge(drainCtx);
    const duplicate = handleSubcBgEventsNudge(drainCtx);

    expect(duplicate).toBe(first);
    expect(send.mock.calls.filter((call) => call[0] === "bash_drain_completions")).toHaveLength(1);
    releaseDrain();
    await Promise.all([first, duplicate]);
  });

  test("undefined session uses the same default key for nudge dedupe and state", async () => {
    let releaseDrain!: () => void;
    const drainGate = new Promise<void>((resolve) => {
      releaseDrain = resolve;
    });
    const send = mock(async (command: string) => {
      if (command === "bash_drain_completions") await drainGate;
      return { success: true, bg_completions: [] };
    });
    const { ctx } = harness(send);
    const runtime = { sendUserMessage: mock(() => {}) };
    const defaultNudge = handleSubcBgEventsNudge({
      ctx,
      directory: "/tmp/project",
      runtime,
    });
    const explicitDefaultNudge = handleSubcBgEventsNudge({
      ctx,
      directory: "/tmp/project",
      sessionID: "__default__",
      runtime,
    });

    expect(explicitDefaultNudge).toBe(defaultNudge);
    expect(sessionBgStates.has("__default__")).toBe(true);
    expect(sessionBgStates.has("undefined")).toBe(false);
    const drainCalls = send.mock.calls.filter((call) => call[0] === "bash_drain_completions");
    expect(drainCalls).toHaveLength(1);
    expect(drainCalls[0]?.[1]).toMatchObject({ session_id: "__default__" });

    releaseDrain();
    await Promise.all([defaultNudge, explicitDefaultNudge]);
  });

  test("a forced drain during the ack window does not double-deliver", async () => {
    // Pi delivery (sendUserMessage) is synchronous; the in-flight window is the
    // ack round-trip. Hold the ack open, fire a subc nudge force-drain in that
    // window, and assert the completion is NOT delivered twice.
    trackBgTask("s1", "task-1");
    let releaseAck!: () => void;
    const ackGate = new Promise<void>((r) => {
      releaseAck = r;
    });
    const send = mock(async (command: string) => {
      if (command === "bash_drain_completions") {
        return { success: true, bg_completions: [completion("task-1", "npm test")] };
      }
      await ackGate; // hold the ack open (delivery already happened)
      return { success: true, acked_task_ids: ["task-1"] };
    });
    const { ctx } = harness(send);
    const sendUserMessage = mock(() => {});
    const drainCtx = {
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      runtime: { sendUserMessage },
    };

    // First delivery: push → wake → sendUserMessage (sync) → ack parks on gate.
    ingestBgCompletions("s1", [completion("task-1", "npm test")]);
    await handleTurnEndBgCompletions(drainCtx);
    await waitForMockCallCount(sendUserMessage, 1);

    // While ack is parked, a subc nudge force-drains (module still holds task-1).
    // Don't await it: its C-#3 re-ack parks on the SAME gate. The double-deliver
    // decision (ingest dedup) happens synchronously before that await, so the
    // assertion below is valid while the nudge is parked.
    const nudge = handleSubcBgEventsNudge(drainCtx);
    await sleep(300);
    // Still exactly one delivery — the in-flight task was skipped, not re-delivered.
    expect(sendUserMessage.mock.calls.length).toBe(1);

    releaseAck();
    await nudge;
    await sleep(50);
    expect(sendUserMessage.mock.calls.length).toBe(1);
  });

  test("a forced drain re-acks a delivered-but-unacked completion (C-#3 close)", async () => {
    trackBgTask("s1", "task-1");
    let ackCalls = 0;
    const send = mock(async (command: string) => {
      if (command === "bash_drain_completions") {
        return { success: true, bg_completions: [completion("task-1", "npm test")] };
      }
      ackCalls += 1;
      return ackCalls === 1
        ? { success: false, message: "ack transport down" }
        : { success: true, acked_task_ids: ["task-1"] };
    });
    const { ctx } = harness(send);
    const sendUserMessage = mock(() => {});
    const drainCtx = {
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      runtime: { sendUserMessage },
    };

    ingestBgCompletions("s1", [completion("task-1", "npm test")]);
    await handleTurnEndBgCompletions(drainCtx);
    await waitForMockCallCount(sendUserMessage, 1);
    await sleep(50);
    expect(sessionBgStates.get("s1")?.deliveredAwaitingAckTaskIds.has("task-1")).toBe(true);

    await handleSubcBgEventsNudge(drainCtx);
    await sleep(50);

    expect(sendUserMessage.mock.calls.length).toBe(1); // re-acked, not re-delivered
    expect(sessionBgStates.get("s1")?.deliveredAwaitingAckTaskIds.has("task-1")).toBe(false);
  });

  test("a fresh nudge re-arms a hard-stopped failed delivery without skip-and-re-ack loss", async () => {
    let rejectInjection = true;
    const sendUserMessage = mock(() => {
      if (rejectInjection) throw new Error("session injection unavailable");
    });
    const send = mock(async (command: string) =>
      command === "bash_drain_completions"
        ? { success: true, bg_completions: [completion("task-stranded", "npm test")] }
        : { success: true, acked_task_ids: ["task-stranded"] },
    );
    const { ctx } = harness(send);
    const drainCtx = {
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      runtime: { sendUserMessage },
    };

    await handleSubcBgEventsNudge(drainCtx);
    await waitUntil(() => sessionBgStates.get("s1")?.wakeHardStopped === true, 10_000);

    const stranded = sessionBgStates.get("s1");
    expect(stranded?.deliveringTaskIds.has("task-stranded")).toBe(false);
    expect(stranded?.deliveredAwaitingAckTaskIds.has("task-stranded")).toBe(false);
    expect(stranded?.pendingCompletions.some((item) => item.task_id === "task-stranded")).toBe(
      true,
    );
    expect(send.mock.calls.filter((call) => call[0] === "bash_ack_completions")).toHaveLength(0);
    expect(
      sessionWarnSpy.mock.calls.filter(
        (call) =>
          (call[2] as { event?: string } | undefined)?.event ===
          "bash_completion_wake_send_user_message_error",
      ),
    ).toHaveLength(1);

    rejectInjection = false;
    await handleSubcBgEventsNudge(drainCtx);
    await waitForMockCallCount(sendUserMessage, 6);
    await waitUntil(
      () => send.mock.calls.filter((call) => call[0] === "bash_ack_completions").length === 1,
    );

    expect(sessionBgStates.get("s1")?.pendingCompletions).toHaveLength(0);
  });

  test("a hung drain times out and releases nudge coalescing for the next drain", async () => {
    __setBgNotificationHopTimeoutForTests(50);
    let hangDrain = true;
    const send = mock(async (command: string) => {
      if (command === "bash_drain_completions" && hangDrain) return new Promise(() => {});
      if (command === "bash_drain_completions") {
        return { success: true, bg_completions: [completion("task-after-hang", "npm test")] };
      }
      return { success: true, acked_task_ids: ["task-after-hang"] };
    });
    const { ctx } = harness(send);
    const sendUserMessage = mock(() => {});
    const drainCtx = {
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      runtime: { sendUserMessage },
    };

    const first = handleSubcBgEventsNudge(drainCtx);
    expect(handleSubcBgEventsNudge(drainCtx)).toBe(first);
    await first;

    hangDrain = false;
    await handleSubcBgEventsNudge(drainCtx);
    await waitForMockCallCount(sendUserMessage, 1);
    await waitUntil(
      () => send.mock.calls.filter((call) => call[0] === "bash_ack_completions").length === 1,
    );
  });
});

function harness(
  sendImpl: (
    command: string,
    params: Record<string, unknown>,
  ) => Promise<BridgeResponse> | BridgeResponse,
) {
  const bridge = {
    send: async (command: string, params: Record<string, unknown>) => sendImpl(command, params),
  };
  const ctx = {
    pool: {
      getActiveBridgeForRoot: () => bridge,
      getBridge: () => bridge,
    },
    config: {},
    storageDir: "/tmp/aft-test",
  } as unknown as PluginContext;
  return { ctx };
}

function completion(task_id: string, command: string) {
  return { task_id, status: "completed", exit_code: 0, command };
}

function ingestCompletionForCleanup(sessionID: string, taskID: string): void {
  const state = sessionBgStates.get(sessionID);
  if (!state) throw new Error(`missing state for ${sessionID}`);
  state.outstandingTaskIds.delete(taskID);
}

async function waitForMockCallCount(
  fn: { mock: { calls: unknown[] } },
  count: number,
  timeoutMs = 5_000,
): Promise<void> {
  await waitUntil(() => fn.mock.calls.length >= count, timeoutMs);
}

async function waitUntil(
  predicate: () => boolean | Promise<boolean>,
  timeoutMs = 5_000,
): Promise<void> {
  const started = Date.now();
  while (!(await predicate())) {
    if (Date.now() - started > timeoutMs) throw new Error("timed out waiting for condition");
    await sleep(50);
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

test("each_line reminder keeps bounded rows and drop recovery hint", () => {
  const reminder = formatPatternMatchReminder([
    {
      task_id: "bash-lines",
      session_id: "s1",
      watch_id: "watch-lines",
      match_text: "matching lines",
      match_offset: 0,
      context:
        'stdout byte 0 ×2: ERROR a\n3 matching lines dropped. Read them with bash_status({ task_id: "bash-lines", output_offset: 16 }).',
      once: false,
      batch: {
        lines: [{ stream: "stdout", offset: 0, end_offset: 8, text: "ERROR a", count: 2 }],
        dropped_lines: 3,
        first_dropped_offset: 16,
        first_dropped_stream: "stdout",
        skipped_long_lines: 0,
      },
    },
  ]);
  expect(reminder).toContain("task bash-lines matching lines:");
  expect(reminder).toContain("stdout byte 0 ×2: ERROR a");
  expect(reminder).toContain(
    '3 matching lines dropped. Read them with bash_status({ task_id: "bash-lines", output_offset: 16 }).',
  );
});

test("each_line queue replaces snapshots without discarding another watch", async () => {
  markTaskWaiting("s-lines", "bash-lines");
  const { ctx } = harness(() => ({ success: true, acked_task_ids: [] }));
  const base = {
    task_id: "bash-lines",
    session_id: "s-lines",
    watch_id: "watch-a",
    match_text: "matching lines",
    match_offset: 0,
    context: "rows",
    once: false,
  };
  const batch = {
    lines: [{ stream: "stdout", offset: 0, end_offset: 8, text: "ERROR a", count: 1 }],
    dropped_lines: 0,
    first_dropped_offset: null,
    first_dropped_stream: null,
    skipped_long_lines: 0,
  };
  const drain = {
    ctx,
    directory: "/tmp/project",
    sessionID: "s-lines",
    runtime: { sendUserMessage: () => {} },
  };
  await handlePushedPatternMatch(drain, { ...base, batch });
  await handlePushedPatternMatch(drain, {
    ...base,
    batch: {
      ...batch,
      snapshot: true,
      dropped_lines: 3,
      first_dropped_offset: 8,
      first_dropped_stream: "stdout",
    },
  });
  await handlePushedPatternMatch(drain, { ...base, watch_id: "watch-b", batch });
  const pending = sessionBgStates.get("s-lines")?.pendingPatternMatches;
  expect(pending).toHaveLength(2);
  expect(pending?.find((entry) => entry.watch_id === "watch-a")?.batch?.dropped_lines).toBe(3);
});

for (const route of ["push", "forced-drain", "restart-replay"] as const) {
  test(`queued each_line batches render once in order with an exact ack (${route})`, async () => {
    const sessionID = "s-queued-lines";
    const frames = [0, 18, 36].map((offset, index) => ({
      task_id: "bash-queued-lines",
      session_id: sessionID,
      watch_id: "watch-queued-lines",
      match_text: "matching lines",
      match_offset: offset,
      context: "unused: render the batch",
      once: false,
      reason: "pattern_match" as const,
      batch: {
        pending_id: "watch-queued-lines:0",
        version: offset,
        lines: [
          {
            stream: "stdout",
            offset,
            end_offset: offset + 18,
            text: `LIVECHECK3 line ${index + 1}`,
            count: 1,
          },
        ],
        dropped_lines: 0,
        first_dropped_offset: null,
        first_dropped_stream: null,
        skipped_long_lines: 0,
      },
    }));
    let pending: PatternMatchEntry[] = [];
    const acks: Record<string, unknown>[] = [];
    const { ctx } = harness((command, params) => {
      if (command === "bash_drain_completions")
        return { success: true, bg_completions: [], pending_matches: pending };
      if (command === "bash_ack_completions") {
        acks.push(params);
        pending = [];
        return { success: true, acked_matches: params.matches ?? [] };
      }
      return { success: true };
    });
    const sendUserMessage = mock(() => {});
    const context = { ctx, directory: "/project", sessionID, runtime: { sendUserMessage } };
    trackBgTask(sessionID, "bash-queued-lines");
    await handleSubcBgEventsNudge(context);
    for (const [index, frame] of frames.entries()) {
      pending = [
        {
          ...frame,
          batch: {
            ...frame.batch,
            snapshot: true,
            lines: frames.slice(0, index + 1).flatMap((entry) => entry.batch.lines),
          },
        },
      ];
      if (route === "forced-drain") await handleSubcBgEventsNudge(context);
      else await handlePushedPatternMatch(context, frame);
      if (route === "restart-replay" && index === 1) {
        __resetBgNotificationStateForTests();
        await handleSubcBgEventsNudge(context);
      }
    }
    expect(sendUserMessage).not.toHaveBeenCalled();
    expect(acks).toEqual([]);
    expect(sessionBgStates.get(sessionID)?.pendingPatternMatches).toMatchObject([
      {
        match_offset: 36,
        batch: {
          lines: [
            { offset: 0, text: "LIVECHECK3 line 1", count: 1 },
            { offset: 18, text: "LIVECHECK3 line 2", count: 1 },
            { offset: 36, text: "LIVECHECK3 line 3", count: 1 },
          ],
        },
      },
    ]);
    const result = await appendToolResultBgCompletions(context, [{ type: "text", text: "tool" }]);
    const reminder = result?.at(-1);
    expect(reminder?.type).toBe("text");
    expect(
      reminder && "text" in reminder
        ? reminder.text.split("\n").filter((line) => line.includes("stdout byte"))
        : [],
    ).toEqual([
      "      > stdout byte 0: LIVECHECK3 line 1",
      "      > stdout byte 18: LIVECHECK3 line 2",
      "      > stdout byte 36: LIVECHECK3 line 3",
    ]);
    expect(acks).toEqual([
      {
        session_id: sessionID,
        matches: [
          { task_id: "bash-queued-lines", watch_id: "watch-queued-lines", match_offset: 36 },
        ],
      },
    ]);
    expect(
      await appendToolResultBgCompletions(context, [{ type: "text", text: "next" }]),
    ).toBeUndefined();
    expect(acks).toHaveLength(1);
  });
}

test("exact pattern ack retries keep sibling watches and newer versions deliverable", async () => {
  const old = {
    task_id: "task-pattern",
    session_id: "s1",
    watch_id: "watch-one",
    match_offset: 40,
    match_text: "READY",
    context: "READY old",
    once: false,
    reason: "pattern_match",
  };
  let pending = [old];
  let confirm = false;
  const calls: Array<{ command: string; params: Record<string, unknown> }> = [];
  const { ctx } = harness((command, params) => {
    calls.push({ command, params });
    if (command === "bash_drain_completions")
      return { success: true, bg_completions: [], pending_matches: pending };
    return { success: true, acked_matches: confirm ? (params.matches ?? []) : [] };
  });
  const context = { ctx, directory: "/tmp/project", sessionID: "s1" };
  const deliver = async () => {
    const content = await appendToolResultBgCompletions(context, [{ type: "text", text: "tool" }]);
    return (
      content
        ?.filter((item) => item.type === "text")
        .map((item) => item.text)
        .join("\n") ?? ""
    );
  };
  expect(await deliver()).toContain("READY old");
  expect(await deliver()).not.toContain("READY old");
  pending = [
    { ...old, watch_id: "watch-two", context: "READY sibling" },
    { ...old, match_offset: 41, context: "READY newer" },
  ];
  confirm = true;
  const next = await deliver();
  expect(next).toContain("READY sibling");
  expect(next).toContain("READY newer");
  expect(next).not.toContain("READY old");
  const acks = calls.filter((call) => call.command === "bash_ack_completions");
  expect(
    acks.some(
      (call) =>
        JSON.stringify(call.params.matches) ===
        JSON.stringify([{ task_id: "task-pattern", watch_id: "watch-one", match_offset: 40 }]),
    ),
  ).toBe(true);
  expect(
    acks.some((call) => Array.isArray(call.params.matches) && call.params.matches.length === 2),
  ).toBe(true);
  for (const call of acks) expect(call.params.task_ids ?? []).not.toContain("task-pattern");
});

for (const route of ["in-turn", "wake"] as const) {
  test(`admitted each_line snapshot rows and counters are not repeated after a failed ack (${route})`, async () => {
    const first = {
      task_id: "task-batch",
      session_id: "s1",
      watch_id: "watch-batch",
      match_offset: 0,
      match_text: "matching lines",
      context: "batch",
      once: false,
      reason: "pattern_match",
      batch: {
        pending_id: "pending-a",
        version: 0,
        snapshot: true,
        lines: [{ stream: "stdout", offset: 0, end_offset: 8, text: "ERROR A", count: 1 }],
        dropped_lines: 3,
        skipped_long_lines: 1,
        first_dropped_offset: 20,
        first_dropped_stream: "stdout",
      },
    };
    let current = first;
    let failAck = true;
    const acknowledgements: Record<string, unknown>[] = [];
    const { ctx } = harness((command, params) => {
      if (command === "bash_drain_completions")
        return { success: true, bg_completions: [], pending_matches: [current] };
      if (command === "bash_ack_completions") {
        acknowledgements.push(params);
        if (failAck) {
          failAck = false;
          return { success: false, message: "ack response lost" };
        }
        return { success: true, acked_matches: params.matches ?? [] };
      }
      return { success: true };
    });
    const sent: string[] = [];
    const runtime = {
      sendUserMessage: (text: string) => {
        sent.push(text);
      },
    };
    const context = { ctx, directory: "/tmp/project", sessionID: "s1", runtime };
    const deliver = async () => {
      if (route === "in-turn") {
        const content = await appendToolResultBgCompletions(context, [
          { type: "text", text: "tool" },
        ]);
        return (
          content
            ?.filter((item) => item.type === "text")
            .map((item) => item.text)
            .join("\n") ?? ""
        );
      }
      const wanted = sent.length + 1;
      await handleSubcBgEventsNudge(context);
      await waitUntil(() => sent.length === wanted && acknowledgements.length >= wanted);
      return sent.at(-1)!;
    };
    const old = await deliver();
    expect(old).toContain("stdout byte 0: ERROR A");
    expect(old).toContain("3 matching lines dropped.");
    expect(old).toContain("1 lines longer than 64 KiB");
    current = {
      ...first,
      match_offset: 1,
      batch: {
        ...first.batch,
        version: 1,
        lines: [
          { ...first.batch.lines[0] },
          { stream: "stderr", offset: 0, end_offset: 8, text: "ERROR B", count: 1 },
        ],
        dropped_lines: 5,
        skipped_long_lines: 2,
      },
    };
    const next = await deliver();
    expect(next).toContain("stderr byte 0: ERROR B");
    expect(next).not.toContain("ERROR A");
    expect(next).toContain("2 matching lines dropped.");
    expect(next).not.toContain("5 matching lines dropped.");
    expect(next).toContain("1 lines longer than 64 KiB");
    expect(next).not.toContain("2 lines longer than 64 KiB");
    expect(acknowledgements.at(-1)).toEqual({
      session_id: "s1",
      matches: [{ task_id: "task-batch", watch_id: "watch-batch", match_offset: 1 }],
    });
  });
}
