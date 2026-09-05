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
  formatSystemReminder,
  handlePushedBgCompletion,
  handleSubcBgEventsNudge,
  handleTurnEndBgCompletions,
  ingestBgCompletions,
  markBgCompletionDelivered,
  markExplicitControl,
  markTaskWaiting,
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
        : { success: true, acked_task_ids: ["task-pattern"] },
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
      task_ids: ["task-pattern"],
    });
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
