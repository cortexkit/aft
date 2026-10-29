/// <reference path="../bun-test.d.ts" />

import { afterAll, afterEach, beforeEach, describe, expect, mock, spyOn, test } from "bun:test";

// Spy on sessionLog/sessionWarn so we can assert on the structured trace
// events emitted by the wake path (event names and
// bash_completion_wake_client_unavailable). The mock MUST be installed
// before the SUT is imported, because Bun hoists `mock.module` to the
// top of the file.
const sessionLogSpy = mock(
  (_sessionID: string | undefined, _message: string, _data?: unknown) => {},
);
const sessionWarnSpy = mock(
  (_sessionID: string | undefined, _message: string, _data?: unknown) => {},
);
const sessionDebugSpy = mock(
  (_sessionID: string | undefined, _message: string, _data?: unknown) => {},
);
mock.module("../logger.js", () => ({
  sessionLog: sessionLogSpy,
  sessionDebug: sessionDebugSpy,
  sessionWarn: sessionWarnSpy,
  log: () => {},
  debug: () => {},
  warn: () => {},
  error: () => {},
  sessionError: () => {},
  bridgeLogger: {
    log: () => {},
    warn: () => {},
    error: () => {},
    getLogFilePath: () => "",
  },
  getLogFilePath: () => "",
  flushLog: () => {},
}));

afterAll(() => {
  mock.restore();
});

import {
  __resetBgNotificationStateForTests,
  __setBgNotificationHopTimeoutForTests,
  __setBusyWakeDeferCeilingForTests,
  __setWakeConfirmationWindowForTests,
  appendInTurnBgCompletions,
  appendPendingWakeRecovery,
  cancelPendingWakesForContext,
  clearSessionBgState,
  consumeBgCompletion,
  formatPatternMatchReminder,
  formatSystemReminder,
  handleIdleBgCompletions,
  handlePushedBgCompletion,
  handlePushedBgLongRunning,
  handlePushedPatternMatch,
  handleSubcBgEventsNudge,
  ingestBgCompletions,
  markBgCompletionDelivered,
  markExplicitControl,
  markTaskWaiting,
  observeOpenCodeBgNotificationEvent,
  type PatternMatchEntry,
  SESSION_BG_STATE_IDLE_TTL_MS,
  sessionBgStates,
  trackBgTask,
} from "../bg-notifications.js";
import type { PluginContext } from "../types.js";

type BridgeResponse = Record<string, unknown>;

beforeEach(() => {
  sessionLogSpy.mockClear();
  sessionDebugSpy.mockClear();
  sessionWarnSpy.mockClear();
});

afterEach(() => {
  __resetBgNotificationStateForTests();
});

/**
 * Build a stub plugin-context client shaped like OpenCode's `input.client`.
 * Returned so individual tests can read `.session.promptAsync.mock.calls`
 * to assert the in-process wake fired.
 */
function makeClient(
  promptAsync: ReturnType<typeof mock>,
  messages?: unknown[],
): { session: { promptAsync: typeof promptAsync; messages?: () => Promise<{ data: unknown[] }> } } {
  return {
    session: {
      promptAsync,
      ...(messages !== undefined ? { messages: async () => ({ data: messages }) } : {}),
    },
  };
}

/** Helper: extract the structured data argument from the first matching trace event. */
function findTraceEvent(eventName: string): Record<string, unknown> | undefined {
  for (const call of sessionLogSpy.mock.calls) {
    const data = call[2] as { event?: string } | undefined;
    if (data?.event === eventName) return data as Record<string, unknown>;
  }
  for (const call of sessionDebugSpy.mock.calls) {
    const data = call[2] as { event?: string } | undefined;
    if (data?.event === eventName) return data as Record<string, unknown>;
  }
  for (const call of sessionWarnSpy.mock.calls) {
    const data = call[2] as { event?: string } | undefined;
    if (data?.event === eventName) return data as Record<string, unknown>;
  }
  return undefined;
}

describe("OpenCode background notifications", () => {
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
  test("PTY incomplete capture completion reminder preserves exit code and shows warning", () => {
    const reason = "PTY output may be incomplete: output drain deadline expired before EOF";
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

  test("formats system reminder bullets with status and duration (no output, no preview block)", () => {
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

  test("formats system reminder with indented output preview when present", () => {
    expect(
      formatSystemReminder([
        {
          task_id: "abc123",
          status: "completed",
          exit_code: 0,
          command: "git status",
          duration_ms: 50,
          output_preview: "On branch main\nnothing to commit, working tree clean\n",
          output_truncated: false,
        },
      ]),
    ).toBe(
      "<system-reminder>\n[BACKGROUND BASH COMPLETED]\n- task abc123 (exit 0, 50ms)\n    On branch main\n    nothing to commit, working tree clean\n</system-reminder>",
    );
  });

  test("formats system reminder with truncation marker and bash_status pointer when truncated", () => {
    const reminder = formatSystemReminder([
      {
        task_id: "xyz789",
        status: "completed",
        exit_code: 1,
        command: "pytest",
        duration_ms: 12_000,
        output_preview: "...rest of trace\nFAILED tests/test_foo.py::test_bar - AssertionError\n",
        output_truncated: true,
      },
    ]);
    expect(reminder).toContain("- task xyz789 (exit 1, 12s)");
    expect(reminder).toContain("    …");
    expect(reminder).toContain("    ...rest of trace");
    expect(reminder).toContain("    FAILED tests/test_foo.py::test_bar - AssertionError");
    expect(reminder).toContain('For truncated tasks, use bash_status({ taskId: "..." })');
  });

  test("strips ANSI escape sequences from output preview", () => {
    const reminder = formatSystemReminder([
      {
        task_id: "ansi1",
        status: "completed",
        exit_code: 0,
        command: "ls --color",
        output_preview: "\x1b[34mfile.txt\x1b[0m\n\x1b[1;32mREADME\x1b[0m\n",
        output_truncated: false,
      },
    ]);
    expect(reminder).toContain("    file.txt");
    expect(reminder).toContain("    README");
    expect(reminder).not.toContain("\x1b[");
  });

  test("blank or whitespace-only preview produces no preview block", () => {
    const reminder = formatSystemReminder([
      {
        task_id: "empty1",
        status: "completed",
        exit_code: 0,
        command: "true",
        output_preview: "   \n\n",
        output_truncated: false,
      },
    ]);
    expect(reminder).toBe(
      "<system-reminder>\n[BACKGROUND BASH COMPLETED]\n- task empty1 (exit 0)\n</system-reminder>",
    );
  });

  test("formats pushed pattern matches with matched framing", () => {
    expect(
      formatPatternMatchReminder([
        {
          task_id: "bash-1",
          session_id: "s1",
          watch_id: "watch-1",
          match_text: "vite-ready-on-port-3000",
          match_offset: 42,
          context: "vite-ready-on-port-3000",
          once: true,
          reason: "pattern_match",
        },
      ]),
    ).toBe(
      '<system-reminder>\n[BG BASH NOTIFY]\n- task bash-1 matched "vite-ready-on-port-3000" (offset 42):\n      > vite-ready-on-port-3000\n</system-reminder>',
    );
  });

  test("formats exit safety-net notifications without matched framing", () => {
    const reminder = formatPatternMatchReminder([
      {
        task_id: "bash-2",
        session_id: "s1",
        watch_id: "exit",
        match_text: "",
        match_offset: 0,
        context: "task bash-2 exited (exit 0)\nvite-ready-on-port-3000",
        once: true,
        reason: "task_exit",
      },
    ]);

    expect(reminder).toContain("- task bash-2 exited:");
    expect(reminder).toContain("task bash-2 exited (exit 0)");
    expect(reminder).toContain("vite-ready-on-port-3000");
    expect(reminder).not.toContain("matched");
    expect(reminder).not.toContain("offset 0");
  });

  test("in-turn delivery drains and appends reminder to tool output", async () => {
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({
      success: true,
      bg_completions: [completion("task-1", "echo done")],
    }));
    const output = { output: "tool output" };

    // In-turn delivery never calls promptAsync.
    await appendInTurnBgCompletions({ ctx, directory: "/tmp/project", sessionID: "s1" }, output);

    expect(output.output).toContain("tool output\n\n<system-reminder>");
    expect(output.output).toContain("- task task-1 (exit 0)");
    expect(output.output).not.toContain(": echo done"); // command no longer in bullet
    expect(sessionBgStates.get("s1")?.pendingCompletions).toHaveLength(0);
    expect(sessionBgStates.get("s1")?.outstandingTaskIds.size).toBe(0);
  });

  test("first no-task path force-drains once for replayed completions", async () => {
    const send = mock(async () => ({ success: true, bg_completions: [] }));
    const { ctx } = harness(send);
    const output = { output: "tool output" };

    await appendInTurnBgCompletions({ ctx, directory: "/tmp/project", sessionID: "s1" }, output);

    expect(send).toHaveBeenCalledTimes(1);
    expect(send.mock.calls[0][0]).toBe("bash_drain_completions");
    expect(output.output).toBe("tool output");
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
      client: {},
      config: {},
      storageDir: "/tmp/aft-test",
    } as unknown as PluginContext;
    const drainContext = { ctx, directory: "/tmp/project", sessionID: "s1" };
    const client = makeClient(mock(async () => {}));

    await appendInTurnBgCompletions(drainContext, { output: "first" });
    await appendInTurnBgCompletions(drainContext, { output: "second" });
    await handleIdleBgCompletions({ ...drainContext, client });

    expect(send).toHaveBeenCalledTimes(0);
    expect(sessionWarnSpy).not.toHaveBeenCalled();
    expect(sessionBgStates.get("s1")?.forcedDrainCompleted).toBe(false);

    activeBridge = bridge;
    await appendInTurnBgCompletions(drainContext, { output: "third" });
    await appendInTurnBgCompletions(drainContext, { output: "fourth" });
    await handleIdleBgCompletions({ ...drainContext, client });

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
      client: {},
      config: {},
      storageDir: "/tmp/aft-test",
    } as unknown as PluginContext;
    trackBgTask("s1", "task-1");

    await appendInTurnBgCompletions(
      { ctx, directory: "/tmp/project", sessionID: "s1" },
      { output: "tool output" },
    );

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
      client: {},
      config: {},
      storageDir: "/tmp/aft-test",
    } as unknown as PluginContext;
    trackBgTask("s1", "task-1");
    ingestBgCompletions("s1", [completion("task-1", "echo done")]);
    const client = makeClient(mock(async () => {}));

    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client,
    });

    expect(
      sessionWarnSpy.mock.calls.some(
        (call) =>
          (call[2] as { event?: string } | undefined)?.event === "bash_completion_drain_error",
      ),
    ).toBe(true);
  });

  test("forced drain delivers replayed completion even when task is not tracked", async () => {
    const send = mock(async (command: string) =>
      command === "bash_drain_completions"
        ? { success: true, bg_completions: [completion("task-1", "echo replayed")] }
        : { success: true, acked_task_ids: ["task-1"] },
    );
    const { ctx } = harness(send);
    const output = { output: "tool output" };

    await appendInTurnBgCompletions({ ctx, directory: "/tmp/project", sessionID: "s1" }, output);

    expect(output.output).toContain("- task task-1 (exit 0)");
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
      const output = { output: `tool output ${i}` };
      await appendInTurnBgCompletions({ ctx, directory: "/tmp/project", sessionID: "s1" }, output);
      expect(output.output).toBe(`tool output ${i}`);
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

    const first = { output: "first" };
    await appendInTurnBgCompletions(drainContext, first);
    expect(first.output).toBe("first");

    // The bash tool spawns a background task, which then finishes with no
    // push frame reaching the plugin; the daemon only holds the completion.
    trackBgTask("s1", "task-1");
    held = [completion("task-1", "sleep 1")];

    const second = { output: "second" };
    await appendInTurnBgCompletions(drainContext, second);
    expect(second.output).toContain("- task task-1 (exit 0)");
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

    await appendInTurnBgCompletions(drainContext, { output: "first" });

    // A completion this process never tracked (spawned before a plugin
    // restart, or by another path) is buffered by the push handler, not
    // delivered; the daemon still holds it unacknowledged.
    held = [completion("task-9", "make")];
    await handlePushedBgCompletion(
      { ...drainContext, client: makeClient(mock(async () => {})) },
      held[0],
    );

    const second = { output: "second" };
    await appendInTurnBgCompletions(drainContext, second);
    expect(second.output).toContain("- task task-9 (exit 0)");
  });

  test("a failed idle drain is retried on the next tool result", async () => {
    let fail = true;
    const send = mock(async () =>
      fail ? { success: false, message: "busy" } : { success: true, bg_completions: [] },
    );
    const { ctx } = harness(send);
    const drainContext = { ctx, directory: "/tmp/project", sessionID: "s1" };

    await appendInTurnBgCompletions(drainContext, { output: "first" });
    fail = false;
    await appendInTurnBgCompletions(drainContext, { output: "second" });
    await appendInTurnBgCompletions(drainContext, { output: "third" });

    expect(send).toHaveBeenCalledTimes(2);
  });

  test("turn-end wake sends one promptAsync message with reminder", async () => {
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({
      success: true,
      bg_completions: [completion("task-1", "npm test")],
    }));
    const promptAsync = mock(async () => {});
    const client = makeClient(promptAsync);

    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client,
    });
    await waitForMockCallCount(promptAsync, 1);

    expect(promptAsync).toHaveBeenCalledTimes(1);
    const payload = promptAsync.mock.calls[0][0] as {
      body: {
        noReply: boolean;
        messageID: string;
        parts: Array<{ text: string; synthetic?: boolean; ignored?: boolean }>;
      };
    };
    expect(payload.body.noReply).toBe(false);
    expect(payload.body.messageID).toMatch(/^msg_[0-9a-f]{12}[0-9A-Za-z]{14}$/);
    expect(payload.body.parts[0].text).toContain("- task task-1 (exit 0)");
    expect(payload.body.parts[0].text).not.toContain(": npm test");
    // #129: the agent-directed wake part MUST be synthetic (model-visible,
    // not a user turn, byte-stable across OpenCode's mid-turn wrapper flip)
    // and MUST NOT be `ignored` (which would strip it from the model call).
    expect(payload.body.parts[0].synthetic).toBe(true);
    expect(payload.body.parts[0].ignored).toBeUndefined();
  });

  test("unknown host event shapes are ignored without throwing", () => {
    const malformedEvents: unknown[] = [
      null,
      [],
      { type: "message.updated", properties: { info: { role: "assistant" } } },
      { type: "session.status", properties: { sessionID: "s1", status: "busy" } },
      { type: "unrelated", properties: { sessionID: "s1" } },
    ];

    for (const event of malformedEvents) {
      expect(() => observeOpenCodeBgNotificationEvent(event)).not.toThrow();
    }
    expect(sessionBgStates.has("s1")).toBe(false);
  });

  test("admitted wake confirms from an assistant child without refiring", async () => {
    __setWakeConfirmationWindowForTests(50);
    observeOpenCodeBgNotificationEvent({
      type: "session.status",
      properties: { sessionID: "s1", status: { type: "idle" } },
    });
    trackBgTask("s1", "task-1");
    const { ctx } = harness((command) =>
      command === "bash_drain_completions"
        ? { success: true, bg_completions: [completion("task-1", "npm test")] }
        : { success: true, acked_task_ids: ["task-1"] },
    );
    const promptAsync = mock(async () => {});

    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: makeClient(promptAsync),
    });
    await waitForMockCallCount(promptAsync, 1);
    const admittedMessageID = (promptAsync.mock.calls[0][0] as { body: { messageID: string } }).body
      .messageID;

    observeOpenCodeBgNotificationEvent({
      type: "message.updated",
      properties: {
        info: {
          id: "msg-assistant-1",
          sessionID: "s1",
          role: "assistant",
          parentID: admittedMessageID,
        },
      },
    });
    await sleep(75);

    expect(promptAsync).toHaveBeenCalledTimes(1);
    expect(findTraceEvent("bash_completion_wake_confirmed")).toEqual(
      expect.objectContaining({
        delivery_id: expect.any(String),
        signal: "assistant_parent",
        assistant_message_id: "msg-assistant-1",
      }),
    );
    expect(findTraceEvent("bash_completion_wake_refired")).toBeUndefined();
  });

  test("unrelated assistant message does not confirm an admitted wake", async () => {
    __setWakeConfirmationWindowForTests(100);
    observeOpenCodeBgNotificationEvent({
      type: "session.status",
      properties: { sessionID: "s1", status: { type: "idle" } },
    });
    trackBgTask("s1", "task-1");
    const { ctx } = harness((command) =>
      command === "bash_drain_completions"
        ? { success: true, bg_completions: [completion("task-1", "npm test")] }
        : { success: true, acked_task_ids: ["task-1"] },
    );
    const promptAsync = mock(async () => {});

    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: makeClient(promptAsync),
    });
    await waitForMockCallCount(promptAsync, 1);
    const admittedMessageID = (promptAsync.mock.calls[0][0] as { body: { messageID: string } }).body
      .messageID;

    observeOpenCodeBgNotificationEvent({
      type: "message.updated",
      properties: {
        info: {
          id: "msg-assistant-unrelated",
          sessionID: "s1",
          role: "assistant",
          parentID: "msg-different-parent",
        },
      },
    });
    expect(findTraceEvent("bash_completion_wake_confirmed")).toBeUndefined();

    observeOpenCodeBgNotificationEvent({
      type: "message.updated",
      properties: {
        info: {
          id: "msg-assistant-related",
          sessionID: "s1",
          role: "assistant",
          parentID: admittedMessageID,
        },
      },
    });
    await sleep(125);

    expect(promptAsync).toHaveBeenCalledTimes(1);
    expect(findTraceEvent("bash_completion_wake_confirmed")).toEqual(
      expect.objectContaining({ assistant_message_id: "msg-assistant-related" }),
    );
  });

  test("admitted wake confirms from an idle-to-busy edge without refiring", async () => {
    __setWakeConfirmationWindowForTests(50);
    observeOpenCodeBgNotificationEvent({
      type: "session.status",
      properties: { sessionID: "s1", status: { type: "idle" } },
    });
    trackBgTask("s1", "task-1");
    const { ctx } = harness((command) =>
      command === "bash_drain_completions"
        ? { success: true, bg_completions: [completion("task-1", "npm test")] }
        : { success: true, acked_task_ids: ["task-1"] },
    );
    const promptAsync = mock(async () => {});

    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: makeClient(promptAsync),
    });
    await waitForMockCallCount(promptAsync, 1);
    observeOpenCodeBgNotificationEvent({
      type: "session.status",
      properties: { sessionID: "s1", status: { type: "busy" } },
    });
    await sleep(75);

    expect(promptAsync).toHaveBeenCalledTimes(1);
    expect(findTraceEvent("bash_completion_wake_confirmed")).toEqual(
      expect.objectContaining({ signal: "idle_busy_edge" }),
    );
    expect(findTraceEvent("bash_completion_wake_refired")).toBeUndefined();
  });

  test("busy admission waits for idle instead of refiring at the confirmation timeout", async () => {
    __setWakeConfirmationWindowForTests(50);
    observeOpenCodeBgNotificationEvent({
      type: "session.status",
      properties: { sessionID: "s1", status: { type: "busy" } },
    });
    trackBgTask("s1", "task-1");
    const { ctx } = harness((command) =>
      command === "bash_drain_completions"
        ? { success: true, bg_completions: [completion("task-1", "npm test")] }
        : { success: true, acked_task_ids: ["task-1"] },
    );
    const promptAsync = mock(async () => {});

    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: makeClient(promptAsync),
    });
    await waitForMockCallCount(promptAsync, 1);
    await sleep(75);

    expect(promptAsync).toHaveBeenCalledTimes(1);
    expect(findTraceEvent("bash_completion_wake_refired")).toBeUndefined();

    observeOpenCodeBgNotificationEvent({
      type: "session.idle",
      properties: { sessionID: "s1" },
    });
    await waitForMockCallCount(promptAsync, 2);
  });

  test("idle-at-admission timeout refires exactly once per delivery", async () => {
    __setWakeConfirmationWindowForTests(50);
    observeOpenCodeBgNotificationEvent({
      type: "session.status",
      properties: { sessionID: "s1", status: { type: "idle" } },
    });
    trackBgTask("s1", "task-1");
    const { ctx } = harness((command) =>
      command === "bash_drain_completions"
        ? { success: true, bg_completions: [completion("task-1", "npm test")] }
        : { success: true, acked_task_ids: ["task-1"] },
    );
    const promptAsync = mock(async () => {});

    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: makeClient(promptAsync),
    });
    await waitForMockCallCount(promptAsync, 1);
    const firstMessageID = (promptAsync.mock.calls[0][0] as { body: { messageID: string } }).body
      .messageID;

    await waitForMockCallCount(promptAsync, 2);
    expect(findTraceEvent("bash_completion_wake_refired")).toEqual(
      expect.objectContaining({ delivery_id: expect.any(String) }),
    );

    observeOpenCodeBgNotificationEvent({
      type: "session.idle",
      properties: { sessionID: "s1" },
    });
    await sleep(25);
    expect(promptAsync).toHaveBeenCalledTimes(2);

    observeOpenCodeBgNotificationEvent({
      type: "message.updated",
      properties: {
        info: {
          id: "msg-assistant-late",
          sessionID: "s1",
          role: "assistant",
          parentID: firstMessageID,
        },
      },
    });
    observeOpenCodeBgNotificationEvent({
      type: "session.idle",
      properties: { sessionID: "s1" },
    });
    await sleep(50);

    expect(promptAsync).toHaveBeenCalledTimes(2);
    expect(sessionBgStates.get("s1")?.refiredWakeDeliveryIds.size).toBe(1);
  });

  test("late assistant child suppresses a queued refire before it commits", async () => {
    observeOpenCodeBgNotificationEvent({
      type: "session.status",
      properties: { sessionID: "s1", status: { type: "busy" } },
    });
    trackBgTask("s1", "task-1");
    const { ctx } = harness((command) =>
      command === "bash_drain_completions"
        ? { success: true, bg_completions: [completion("task-1", "npm test")] }
        : { success: true, acked_task_ids: ["task-1"] },
    );
    const promptAsync = mock(async () => {});

    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: makeClient(promptAsync),
    });
    await waitForMockCallCount(promptAsync, 1);
    const admittedMessageID = (promptAsync.mock.calls[0][0] as { body: { messageID: string } }).body
      .messageID;

    observeOpenCodeBgNotificationEvent({
      type: "session.idle",
      properties: { sessionID: "s1" },
    });
    observeOpenCodeBgNotificationEvent({
      type: "message.updated",
      properties: {
        info: {
          id: "msg-assistant-raced",
          sessionID: "s1",
          role: "assistant",
          parentID: admittedMessageID,
        },
      },
    });
    await sleep(25);

    expect(promptAsync).toHaveBeenCalledTimes(1);
    expect(findTraceEvent("bash_completion_wake_refire_suppressed")).toEqual(
      expect.objectContaining({
        delivery_id: expect.any(String),
        cause: "turn_observed_before_refire",
      }),
    );
  });

  test("promptAsync pre-admission rejection does not create a confirmation refire", async () => {
    observeOpenCodeBgNotificationEvent({
      type: "session.status",
      properties: { sessionID: "s1", status: { type: "idle" } },
    });
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({ success: true, bg_completions: [] }));
    const promptAsync = mock(async () => {
      return preAdmissionRejection("send failed");
    });

    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: makeClient(promptAsync),
    });
    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client: makeClient(promptAsync),
      },
      completion("task-1", "npm test"),
    );
    await waitForMockCallCount(promptAsync, 1);

    expect(sessionBgStates.get("s1")?.wakeAdmissions.size).toBe(0);
    expect(findTraceEvent("bash_completion_wake_refired")).toBeUndefined();
    expect(findTraceEvent("bash_completion_wake_refire_suppressed")).toBeUndefined();
    expect(sessionBgStates.get("s1")?.pendingCompletions).toHaveLength(1);
  });

  test("turn-end wake invokes promptAsync on its receiver, as the generated SDK requires (#297)", async () => {
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({
      success: true,
      bg_completions: [completion("task-1", "npm test")],
    }));
    // Mirrors packages/sdk/js/src/gen/sdk.gen.ts: the method reads
    // `this._client`, so a detached call throws before any request is made.
    // An arrow-function stub cannot catch that; this class can.
    const posted: unknown[] = [];
    class SessionApi {
      _client = { post: async (input: unknown) => posted.push(input) };
      promptAsync(input: unknown) {
        return this._client.post(input);
      }
    }
    const client = { session: new SessionApi() };

    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client,
    });
    const deadline = Date.now() + 5_000;
    while (posted.length < 1 && Date.now() < deadline) {
      await new Promise((resolve) => setTimeout(resolve, 10));
    }

    expect(posted).toHaveLength(1);
    expect(findTraceEvent("bash_completion_wake_prompt_async_error")).toBeUndefined();
  });

  test("turn-end wake forwards resolved agent + model + variant to preserve prefix cache", async () => {
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({
      success: true,
      bg_completions: [completion("task-1", "npm test")],
    }));
    const promptAsync = mock(async () => {});
    const client = makeClient(promptAsync, [
      {
        info: {
          role: "assistant",
          agent: "build",
          providerID: "anthropic",
          modelID: "claude-opus-4-7",
          variant: "thinking",
        },
      },
    ]);

    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client,
    });
    await waitForMockCallCount(promptAsync, 1);

    expect(promptAsync).toHaveBeenCalledTimes(1);
    const payload = promptAsync.mock.calls[0][0] as {
      body: {
        noReply: boolean;
        parts: Array<{ text: string }>;
        agent?: string;
        model?: { providerID: string; modelID: string };
        variant?: string;
      };
    };
    expect(payload.body.agent).toBe("build");
    expect(payload.body.model).toEqual({
      providerID: "anthropic",
      modelID: "claude-opus-4-7",
    });
    expect(payload.body.variant).toBe("thinking");
  });

  test("turn-end wake omits model/variant when no prior message provides one", async () => {
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({
      success: true,
      bg_completions: [completion("task-1", "npm test")],
    }));
    const promptAsync = mock(async () => {});
    // Empty session — no prior messages, so the resolver returns null and
    // the wake should go out without forging a fake model.
    const client = makeClient(promptAsync, []);

    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client,
    });
    await waitForMockCallCount(promptAsync, 1);

    expect(promptAsync).toHaveBeenCalledTimes(1);
    const payload = promptAsync.mock.calls[0][0] as {
      body: {
        noReply: boolean;
        parts: Array<{ text: string }>;
        agent?: unknown;
        model?: unknown;
        variant?: unknown;
      };
    };
    expect(payload.body.agent).toBeUndefined();
    expect(payload.body.model).toBeUndefined();
    expect(payload.body.variant).toBeUndefined();
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

  test("retroactively converted task-exit notify is acked after in-turn delivery", async () => {
    trackBgTask("s1", "task-1");
    ingestBgCompletions("s1", [completion("task-1", "sleep 3 && echo X")]);
    markExplicitControl("s1", "task-1", false);
    const send = mock(async (command: string) =>
      command === "bash_ack_completions"
        ? { success: true, acked_task_ids: ["task-1"] }
        : { success: true, bg_completions: [] },
    );
    const { ctx } = harness(send);
    const output = { output: "watch registered" };

    await appendInTurnBgCompletions({ ctx, directory: "/tmp/project", sessionID: "s1" }, output);

    expect(output.output).toContain("[BG BASH NOTIFY]");
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

    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client: {},
      },
      completion("task-1", "echo READY"),
    );
    markExplicitControl("s1", "task-1", false);
    markExplicitControl("s1", "task-1");

    const output = { output: "watch registered" };
    await appendInTurnBgCompletions({ ctx, directory: "/tmp/project", sessionID: "s1" }, output);

    expect(output.output).toContain("[BG BASH NOTIFY]");
    expect(output.output).not.toContain("[BACKGROUND BASH COMPLETED]");
    expect(output.output?.match(/- task task-1 exited:/g)).toHaveLength(1);
  });

  test("push completion lands in pending and wakes after the spawn turn is idle", async () => {
    trackBgTask("s1", "task-1");
    const send = mock(async () => ({
      success: true,
      bg_completions: [],
      acked_task_ids: ["task-1"],
    }));
    const { ctx } = harness(send);
    const promptAsync = mock(async () => {});
    const client = makeClient(promptAsync);
    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client,
    });

    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client,
      },
      completion("task-1", "npm test"),
    );
    await waitForMockCallCount(promptAsync, 1);

    expect(promptAsync).toHaveBeenCalledTimes(1);
    const text = (promptAsync.mock.calls[0][0] as { body: { parts: Array<{ text: string }> } }).body
      .parts[0].text;
    expect(text).toContain("- task task-1 (exit 0)");
    expect(text).not.toContain(": npm test");
    expect(sessionBgStates.get("s1")?.pendingCompletions).toHaveLength(0);
    expect(send.mock.calls.some((call) => call[0] === "bash_ack_completions")).toBe(true);
  });

  test("busy session holds an older completion for in-turn or idle delivery", async () => {
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({
      success: true,
      bg_completions: [completion("task-1", "npm test")],
      acked_task_ids: ["task-1"],
    }));
    const promptAsync = mock(async () => {});
    const client = makeClient(promptAsync);
    const state = sessionBgStates.get("s1");
    if (!state) throw new Error("tracked session state missing");
    state.wakeDeferredTaskIds.clear();
    observeOpenCodeBgNotificationEvent({
      type: "session.status",
      properties: { sessionID: "s1", status: { type: "busy" } },
    });

    await handleSubcBgEventsNudge({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client,
    });
    await sleep(300);

    expect(promptAsync).toHaveBeenCalledTimes(0);
    expect(state.pendingCompletions).toHaveLength(1);

    observeOpenCodeBgNotificationEvent({
      type: "session.idle",
      properties: { sessionID: "s1" },
    });
    await waitForMockCallCount(promptAsync, 1);

    expect(promptAsync).toHaveBeenCalledTimes(1);
    const text = (promptAsync.mock.calls[0][0] as { body: { parts: Array<{ text: string }> } }).body
      .parts[0].text;
    expect(text).toContain("- task task-1 (exit 0)");
  });

  test("busy session wakes once at the defer ceiling when no idle event arrives", async () => {
    __setBusyWakeDeferCeilingForTests(25);
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({
      success: true,
      bg_completions: [completion("task-1", "npm test")],
      acked_task_ids: ["task-1"],
    }));
    const promptAsync = mock(async () => {});
    const client = makeClient(promptAsync);
    const state = sessionBgStates.get("s1");
    if (!state) throw new Error("tracked session state missing");
    state.wakeDeferredTaskIds.clear();
    observeOpenCodeBgNotificationEvent({
      type: "session.status",
      properties: { sessionID: "s1", status: { type: "busy" } },
    });

    await handleSubcBgEventsNudge({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client,
    });
    await sleep(10);
    expect(promptAsync).toHaveBeenCalledTimes(0);

    await waitForMockCallCount(promptAsync, 1, 1_000);
    await sleep(250);

    expect(promptAsync).toHaveBeenCalledTimes(1);
    expect(findTraceEvent("bash_completion_wake_deferred_ceiling")).toEqual(
      expect.objectContaining({
        wait_ms: expect.any(Number),
        ceiling_ms: 25,
      }),
    );
  });

  test("same-turn push completion waits for sync bash_watch instead of waking", async () => {
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({ success: true, bg_completions: [] }));
    const promptAsync = mock(async () => {});
    const client = makeClient(promptAsync);

    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client,
      },
      completion("task-1", "npm test"),
    );
    await sleep(300);

    expect(promptAsync).toHaveBeenCalledTimes(0);
    expect(sessionBgStates.get("s1")?.pendingCompletions).toHaveLength(1);
    expect(sessionBgStates.get("s1")?.debounceTimer).toBeNull();

    markTaskWaiting("s1", "task-1");
    await sleep(300);

    expect(promptAsync).toHaveBeenCalledTimes(0);
    expect(sessionBgStates.get("s1")?.pendingCompletions).toHaveLength(0);
  });

  test("buffers push completion received before task tracking", async () => {
    const { ctx } = harness(() => ({ success: true, bg_completions: [] }));
    const promptAsync = mock(async () => {});
    const client = makeClient(promptAsync);

    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client,
      },
      completion("task-1", "npm test"),
    );
    trackBgTask("s1", "task-1");
    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client,
    });
    await waitForMockCallCount(promptAsync, 1);

    expect(promptAsync).toHaveBeenCalledTimes(1);
    const text = (promptAsync.mock.calls[0][0] as { body: { parts: Array<{ text: string }> } }).body
      .parts[0].text;
    expect(text).toContain("- task task-1 (exit 0)");
  });

  test("rejected wake keeps pending completions and retries", async () => {
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({ success: true, bg_completions: [] }));
    const promptAsync = mock(async () => {
      return preAdmissionRejection("send failed");
    });
    const fallbackClient = makeClient(promptAsync);
    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: fallbackClient,
    });

    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client: fallbackClient,
      },
      completion("task-1", "npm test"),
    );
    await waitForMockCallCount(promptAsync, 1);

    expect(promptAsync).toHaveBeenCalledTimes(1);
    expect(sessionBgStates.get("s1")?.pendingCompletions).toHaveLength(1);
    expect(sessionBgStates.get("s1")?.debounceTimer).not.toBeNull();
  });

  test("rejected wake hard-stops after capped retries", async () => {
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({ success: true, bg_completions: [] }));
    const promptAsync = mock(async () => {
      return preAdmissionRejection("send failed");
    });
    const fallbackClient = makeClient(promptAsync);
    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: fallbackClient,
    });

    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client: fallbackClient,
      },
      completion("task-1", "npm test"),
    );
    await waitUntil(
      () => promptAsync.mock.calls.length >= 5 && sessionBgStates.get("s1")?.debounceTimer === null,
      10_000,
    );

    expect(promptAsync).toHaveBeenCalledTimes(5);
    expect(sessionBgStates.get("s1")?.pendingCompletions).toHaveLength(1);
    expect(sessionBgStates.get("s1")?.debounceTimer).toBeNull();
  });

  test("post-idle push completion still wakes even when bridge is busy with non-agent RPC", async () => {
    // Regression: previously bailed on `isActive()` (bridge.hasPendingRequests())
    // which returned true for the TUI status poll, orphaning the completion when
    // no other trigger fired. Once the spawn turn has gone idle, the wake must
    // still be scheduled.
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({ success: true, bg_completions: [] }));
    const promptAsync = mock(async () => {});
    const client = makeClient(promptAsync);
    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client,
    });

    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client,
      },
      completion("task-1", "npm test"),
    );
    await waitForMockCallCount(promptAsync, 1);

    expect(promptAsync).toHaveBeenCalledTimes(1);
    const text = (promptAsync.mock.calls[0][0] as { body: { parts: Array<{ text: string }> } }).body
      .parts[0].text;
    expect(text).toContain("task-1");
    expect(sessionBgStates.get("s1")?.pendingCompletions).toHaveLength(0);
  });

  test("coalesces three idle completions into one notification", async () => {
    const responses = [
      { success: true, bg_completions: [completion("task-1", "one")] },
      { success: true, bg_completions: [completion("task-2", "two")] },
      { success: true, bg_completions: [completion("task-3", "three")] },
    ];
    const { ctx } = harness(() => responses.shift() ?? { success: true, bg_completions: [] });
    const promptAsync = mock(async () => {});
    const client = makeClient(promptAsync);

    for (const taskId of ["task-1", "task-2", "task-3"]) trackBgTask("s1", taskId);
    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client,
    });
    await sleep(50);
    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client,
    });
    await sleep(50);
    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client,
    });
    await waitForMockCallCount(promptAsync, 1);

    expect(promptAsync).toHaveBeenCalledTimes(1);
    const text = (promptAsync.mock.calls[0][0] as { body: { parts: Array<{ text: string }> } }).body
      .parts[0].text;
    expect(text.match(/^- task/gm)).toHaveLength(3);
  });

  test("debounce cap forces wake before the ticking finishes", async () => {
    // Contract under test: when completions arrive faster than the
    // debounce step window, the cap (DEBOUNCE_CAP_MS = 1000ms in
    // bg-notifications.ts) must fire at least one wake before the ticking
    // would naturally settle. Previously this asserted "exactly 1 wake
    // within wall-clock 950-1400ms"; both bounds were brittle under
    // release.sh's parallel test load (saw 1365ms total + 2 wakes when the
    // cap fired mid-tick-sequence and a trailing tick spawned a second
    // wake). The behavior the cap exists to prevent is "infinite reset"
    // — at least one wake MUST happen during the tick window. That's
    // what we check now.
    let index = 0;
    const { ctx } = harness(() => ({
      success: true,
      bg_completions: [completion(`task-${++index}`, `cmd-${index}`)],
    }));
    const promptAsync = mock(async () => {});
    const client = makeClient(promptAsync);
    const started = Date.now();

    for (let task = 1; task <= 6; task++) trackBgTask("s1", `task-${task}`);
    for (let tick = 0; tick < 6; tick++) {
      await handleIdleBgCompletions({
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client,
      });
      await sleep(190);
    }
    await sleep(120);

    // At least one wake fired during the tick sequence. Without the cap
    // every tick would reset the debounce timer and no wake would ever
    // fire until the final 120ms tail. Under load multiple wakes can
    // fire (cap + trailing ticks), which is fine — what matters is the
    // cap engaged at all.
    expect(promptAsync.mock.calls.length).toBeGreaterThanOrEqual(1);
    // Lower bound proves the cap actually delayed wakes past ~1s
    // instead of firing instantly on the first completion.
    expect(Date.now() - started).toBeGreaterThanOrEqual(950);
  });

  test("second pushed background completion wakes without chat message reset", async () => {
    const promptAsync = mock(async () => {});
    const client = makeClient(promptAsync);
    let responses: BridgeResponse[] = [
      { success: true, bg_completions: [completion("task-1", "one")] },
    ];
    const { ctx } = harness(() => responses.shift() ?? { success: true, bg_completions: [] });

    trackBgTask("s1", "task-1");
    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client,
    });
    await waitForMockCallCount(promptAsync, 1);
    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client,
    });
    expect(sessionBgStates.get("s1")?.debounceTimer ?? null).toBeNull();
    expect(promptAsync).toHaveBeenCalledTimes(1);

    responses = [{ success: true, bg_completions: [completion("task-2", "two")] }];
    trackBgTask("s1", "task-2");
    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client,
    });
    await waitForMockCallCount(promptAsync, 2);
    expect(promptAsync).toHaveBeenCalledTimes(2);
  });

  test("multi-session state is isolated", async () => {
    const { ctx } = harness((_, params) => ({
      success: true,
      bg_completions: [
        completion(params.session_id === "s1" ? "task-1" : "task-2", String(params.session_id)),
      ],
    }));
    const out1 = { output: "one" };
    const out2 = { output: "two" };

    trackBgTask("s1", "task-1");
    trackBgTask("s2", "task-2");
    await appendInTurnBgCompletions({ ctx, directory: "/tmp/project", sessionID: "s1" }, out1);

    expect(out1.output).toContain("task-1");
    expect(out1.output).not.toContain("task-2");
    expect(sessionBgStates.get("s2")?.outstandingTaskIds.has("task-2")).toBe(true);

    await appendInTurnBgCompletions({ ctx, directory: "/tmp/project", sessionID: "s2" }, out2);
    expect(out2.output).toContain("task-2");
  });

  test("drain failure does not break normal tool output", async () => {
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => {
      throw new Error("bridge down");
    });
    const output = { output: "normal" };

    await appendInTurnBgCompletions({ ctx, directory: "/tmp/project", sessionID: "s1" }, output);

    expect(output.output).toBe("normal");
  });

  test("evicts task-free sessions after idle TTL on next access", () => {
    const originalDateNow = Date.now;
    let now = 1_000;
    Date.now = () => now;

    try {
      trackBgTask("stale", "task-1");
      ingestBgCompletions("stale", [completion("task-1", "done")]);
      expect(sessionBgStates.get("stale")?.outstandingTaskIds.size).toBe(0);
      // A finished task still has a queued reminder until delivery consumes it.
      consumeBgCompletion("stale", "task-1");

      now += SESSION_BG_STATE_IDLE_TTL_MS + 1;
      trackBgTask("active", "task-2");

      expect(sessionBgStates.has("stale")).toBe(false);
      expect(sessionBgStates.has("active")).toBe(true);
    } finally {
      Date.now = originalDateNow;
    }
  });

  test("idle eviction retains pending and in-flight notification work", () => {
    const originalDateNow = Date.now;
    let now = 1_000;
    Date.now = () => now;
    try {
      const fields = [
        "pendingCompletions",
        "pendingLongRunning",
        "pendingPatternMatches",
        "deliveringTaskIds",
        "deliveredAwaitingAckTaskIds",
        "wakeDeferredTaskIds",
        "unknownCompletions",
      ] as const;
      for (const field of fields) {
        trackBgTask(field, "task");
        const state = sessionBgStates.get(field)!;
        state.outstandingTaskIds.clear();
        const work = state[field];
        if (work instanceof Set) work.add("task");
        else (work as unknown[]).push({ task_id: "task" });
      }
      now += SESSION_BG_STATE_IDLE_TTL_MS + 1;
      trackBgTask("trigger", "task");
      for (const field of fields) expect(sessionBgStates.has(field)).toBe(true);
    } finally {
      Date.now = originalDateNow;
    }
  });

  test("state access throttles full idle sweeps within the cleanup interval", () => {
    const scan = spyOn(sessionBgStates, Symbol.iterator);
    try {
      for (let i = 0; i < 100; i++) trackBgTask("same-session", "task");
      expect(scan.mock.calls.length).toBeLessThanOrEqual(1);
    } finally {
      scan.mockRestore();
    }
  });

  test("does not evict sessions with outstanding tasks regardless of age", () => {
    const originalDateNow = Date.now;
    let now = 1_000;
    Date.now = () => now;

    try {
      trackBgTask("old-active", "task-1");

      now += SESSION_BG_STATE_IDLE_TTL_MS + 1;
      trackBgTask("new-active", "task-2");

      expect(sessionBgStates.get("old-active")?.outstandingTaskIds.has("task-1")).toBe(true);
      expect(sessionBgStates.has("new-active")).toBe(true);
    } finally {
      Date.now = originalDateNow;
    }
  });

  test("turn-end wake uses drainContext.client for promptAsync delivery", async () => {
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({
      success: true,
      bg_completions: [completion("task-1", "npm test")],
    }));
    const client = makeClient(mock(async () => {}));

    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client,
    });
    await waitForMockCallCount(client.session.promptAsync, 1);

    expect(client.session.promptAsync).toHaveBeenCalledTimes(1);

    const startMeta = findTraceEvent("bash_completion_wake_prompt_async_start");
    expect(startMeta).toBeDefined();
    expect(typeof startMeta?.delivery_id).toBe("string");
    expect(startMeta?.task_ids).toEqual(["task-1"]);
  });

  test("wake without in-process client emits diagnostic and queues for retry", async () => {
    // If the drainContext somehow arrives without the plugin-provided
    // in-process client, the wake has no transport. The path emits a
    // dedicated trace event, holds completions for retry, and lets the
    // existing retry-with-backoff fire.
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({ success: true, bg_completions: [] }));
    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: makeClient(mock(async () => {})),
    });

    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        // client intentionally omitted
      },
      completion("task-1", "npm test"),
    );
    await waitUntil(() => findTraceEvent("bash_completion_wake_client_unavailable") !== undefined);

    // The pending completion is held for retry.
    expect(sessionBgStates.get("s1")?.pendingCompletions).toHaveLength(1);
    // The new diagnostic event names the transport gap.
    const meta = findTraceEvent("bash_completion_wake_client_unavailable");
    expect(meta).toBeDefined();
    expect(meta?.task_ids).toEqual(["task-1"]);
    expect(meta?.attempt).toBe(1);
    expect(sessionBgStates.get("s1")?.debounceTimer).not.toBeNull();
  });

  test("drops pushed pattern match frames for a foreign session before promptAsync", async () => {
    const bridgeCalls: Array<{ command: string; params: Record<string, unknown> }> = [];
    const send = mock(async (command: string, params: Record<string, unknown>) => {
      bridgeCalls.push({ command, params });
      return { success: true };
    });
    const { ctx } = harness(send);
    const promptAsync = mock(async () => {});

    await handlePushedPatternMatch(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "live-session",
        client: { session: { promptAsync } },
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
    expect(promptAsync).not.toHaveBeenCalled();
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
    const promptAsync = mock(async () => {});

    await handlePushedPatternMatch(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "session-live",
        client: { session: { promptAsync } },
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

    await waitForMockCallCount(promptAsync, 1);
    expect(promptAsync).toHaveBeenCalled();

    const loggedHop = sessionLogSpy.mock.calls.find(
      (call) =>
        typeof call[1] === "string" &&
        call[1].includes("session inject start") &&
        (call[2] as Record<string, unknown>)?.watch_id === "watch-pattern-only",
    );
    expect(loggedHop).toBeDefined();
  });
});

describe("subc forced-drain dedup (C-#1 / C-#3)", () => {
  for (const { includeOtherTask, returnPending, name } of [
    {
      includeOtherTask: false,
      returnPending: false,
      name: "inline watch consumption cancels its in-flight wake without restoring exit or still-running reminders",
    },
    {
      includeOtherTask: true,
      returnPending: false,
      name: "wake disposal restores unconsumed tasks but not inline-consumed reminders from the same batch",
    },
    {
      includeOtherTask: true,
      returnPending: true,
      name: "inline watch consumption removes restored reminders without discarding another pending task",
    },
  ]) {
    test(name, async () => {
      const clock = controlledWakeTimers();
      __setBgNotificationHopTimeoutForTests(50);
      const promptAsync = mock(() => new Promise(() => {}));
      const send = mock(async (command: string) =>
        command === "bash_drain_completions"
          ? { success: true, bg_completions: [] }
          : { success: true, acked_task_ids: ["task-other"] },
      );
      const { ctx } = harness(send);
      const sessionID = "inline-watch";
      const drainCtx = {
        ctx,
        directory: "/project",
        sessionID,
        client: makeClient(promptAsync, []),
      };
      const tasks = includeOtherTask ? ["task-covered", "task-other"] : ["task-covered"];
      try {
        for (const task_id of tasks) {
          await handlePushedPatternMatch(drainCtx, {
            task_id,
            session_id: sessionID,
            watch_id: `watch-${task_id}`,
            match_text: "",
            match_offset: 0,
            context: "",
            once: true,
            reason: "task_exit",
          });
          await handlePushedBgLongRunning(drainCtx, {
            task_id,
            session_id: sessionID,
            command: "npm test",
            elapsed_ms: 60_000,
          });
        }
        await clock.fire(tasks.length * 400);
        expect(promptAsync).toHaveBeenCalledTimes(1);
        const request = promptAsync.mock.calls[0][0] as { signal: AbortSignal };
        const state = sessionBgStates.get(sessionID)!;
        expect(state.pendingPatternMatches).toHaveLength(0);
        expect(state.pendingLongRunning).toHaveLength(0);

        if (returnPending) {
          await clock.fire(50);
          await clock.fire(120_000);
          await clock.fire(50);
          expect(state.pendingPatternMatches.map((item) => item.task_id)).toEqual(tasks);
          expect(state.pendingLongRunning.map((item) => item.task_id)).toEqual(tasks);
        }

        consumeBgCompletion(sessionID, "task-covered");
        await clock.flush();
        if (includeOtherTask && !returnPending) {
          // The other task still needs delivery, so inline consumption alone must not cancel it.
          expect(request.signal.aborted).toBe(false);
          cancelPendingWakesForContext(ctx);
          await clock.flush();
        }
        expect(request.signal.aborted).toBe(true);
        expect(state.pendingWakeAdmissions.size).toBe(returnPending ? 1 : 0);
        const remaining = includeOtherTask ? ["task-other"] : [];
        expect(state.pendingLongRunning.map((item) => item.task_id)).toEqual(remaining);
        expect(state.pendingPatternMatches.map((item) => item.task_id)).toEqual(remaining);

        const nextReply = { output: "next tool result" };
        await appendInTurnBgCompletions(drainCtx, nextReply);
        expect(state.pendingWakeAdmissions.size).toBe(0);
        expect(nextReply.output).not.toContain("task-covered");
        if (includeOtherTask) {
          expect(nextReply.output).toContain("task-other");
          expect(nextReply.output).toContain("task task-other exited:");
          expect(nextReply.output).toContain("[BACKGROUND BASH STILL RUNNING]");
        } else {
          expect(nextReply.output).toBe("next tool result");
        }
        const laterReply = { output: "later tool result" };
        await appendInTurnBgCompletions(drainCtx, laterReply);
        expect(laterReply.output).toBe("later tool result");
        await handleIdleBgCompletions(drainCtx);
        await clock.fireIfPresent(200);
        await clock.fireIfPresent(120_000);
        expect(promptAsync).toHaveBeenCalledTimes(returnPending ? 2 : 1);
      } finally {
        __resetBgNotificationStateForTests();
        clock.restore();
      }
    });
  }

  test("deleted session cancels ambiguous SDK requests without recovery or acknowledgement", async () => {
    const clock = controlledWakeTimers();
    __setBgNotificationHopTimeoutForTests(50);
    const promptAsync = mock(() => new Promise(() => {}));
    const send = mock(async (command: string) =>
      command === "bash_drain_completions"
        ? { success: true, bg_completions: [completion("task-deleted", "npm test")] }
        : { success: true },
    );
    const { ctx } = harness(send);
    try {
      await handleSubcBgEventsNudge({
        ctx,
        directory: "/project",
        sessionID: "deleted",
        client: makeClient(promptAsync, []),
      });
      await clock.fire(200);
      const request = promptAsync.mock.calls[0][0] as { signal: AbortSignal };
      clearSessionBgState("deleted");
      await clock.flush();
      expect(request.signal.aborted).toBe(true);
      expect(sessionBgStates.has("deleted")).toBe(false);
      await clock.fireIfPresent(50);
      await clock.fireIfPresent(120_000);
      expect(promptAsync).toHaveBeenCalledTimes(1);
      expect(send.mock.calls.filter((call) => call[0] === "bash_ack_completions")).toHaveLength(0);
    } finally {
      __resetBgNotificationStateForTests();
      clock.restore();
    }
  });

  test("ambiguous V1 wake timeout retains one identity and waits for late acceptance before ack", async () => {
    const clock = controlledWakeTimers();
    __setBgNotificationHopTimeoutForTests(50);
    let accept!: (value: unknown) => void;
    const operation = new Promise((resolve) => {
      accept = resolve;
    });
    const promptAsync = mock(() => operation);
    const client = makeClient(promptAsync, [
      { info: { role: "assistant", agent: "build", providerID: "host", modelID: "previous" } },
    ]);
    const send = mock(async (command: string) =>
      command === "bash_drain_completions"
        ? { success: true, bg_completions: [completion("task-ambiguous", "npm test")] }
        : { success: true, acked_task_ids: ["task-ambiguous"] },
    );
    const { ctx } = harness(send);
    const drainCtx = { ctx, directory: "/project", sessionID: "ambiguous", client };
    try {
      await handleSubcBgEventsNudge(drainCtx);
      await clock.fire(200);
      expect(promptAsync).toHaveBeenCalledTimes(1);
      const original = promptAsync.mock.calls[0][0] as {
        body: { messageID: string };
        signal?: AbortSignal;
      };
      const deliveryID = findTraceEvent("bash_completion_wake_prompt_async_start")?.delivery_id;
      await clock.fire(50);
      expect(promptAsync).toHaveBeenCalledTimes(1);
      expect(original.signal?.aborted).toBe(true);
      expect(sessionBgStates.get("ambiguous")?.deliveringTaskIds.has("task-ambiguous")).toBe(true);
      expect(send.mock.calls.filter((call) => call[0] === "bash_ack_completions")).toHaveLength(0);

      await handleSubcBgEventsNudge(drainCtx);
      observeOpenCodeBgNotificationEvent({
        type: "session.status",
        properties: { sessionID: "ambiguous", status: { type: "busy" } },
      });
      observeOpenCodeBgNotificationEvent({
        type: "session.idle",
        properties: { sessionID: "ambiguous" },
      });
      await clock.flush();
      expect(promptAsync).toHaveBeenCalledTimes(1);
      expect(send.mock.calls.filter((call) => call[0] === "bash_ack_completions")).toHaveLength(0);

      accept({ response: { status: 204 } });
      await clock.flush();
      const acks = send.mock.calls.filter((call) => call[0] === "bash_ack_completions");
      expect(acks).toHaveLength(1);
      expect(acks[0][1]).toMatchObject({ task_ids: ["task-ambiguous"] });
      expect(findTraceEvent("bash_completion_ack_ok")?.delivery_id).toBe(deliveryID);
      observeOpenCodeBgNotificationEvent({
        type: "session.idle",
        properties: { sessionID: "ambiguous" },
      });
      await clock.fireIfPresent(0);
      await clock.fireIfPresent(5000);
      expect(promptAsync).toHaveBeenCalledTimes(1);
      expect((promptAsync.mock.calls[0][0] as typeof original).body.messageID).toBe(
        original.body.messageID,
      );
    } finally {
      __resetBgNotificationStateForTests();
      clock.restore();
    }
  });

  test("unprovable V1 wake recovers once after the window and remains available in the next model turn", async () => {
    const clock = controlledWakeTimers();
    __setBgNotificationHopTimeoutForTests(50);
    const promptAsync = mock(() => new Promise(() => {}));
    const client = makeClient(promptAsync, []);
    const send = mock(async (command: string) =>
      command === "bash_drain_completions"
        ? { success: true, bg_completions: [completion("task-unproven", "npm test")] }
        : { success: true, acked_task_ids: ["task-unproven"] },
    );
    const { ctx } = harness(send);
    const drainCtx = { ctx, directory: "/project", sessionID: "unproven", client };
    try {
      await handleSubcBgEventsNudge(drainCtx);
      await clock.fire(200);
      const original = promptAsync.mock.calls[0][0] as { body: unknown };
      await clock.fire(50);
      await clock.fire(2000);
      expect(promptAsync).toHaveBeenCalledTimes(1);
      expect(send.mock.calls.filter((call) => call[0] === "bash_ack_completions")).toHaveLength(0);

      await clock.fire(120_000);
      expect(promptAsync).toHaveBeenCalledTimes(2);
      expect((promptAsync.mock.calls[1][0] as { body: unknown }).body).toEqual(original.body);
      const warnings = sessionWarnSpy.mock.calls.filter(
        (call) => (call[2] as { event?: string })?.event === "bash_completion_wake_recovery",
      );
      expect(warnings).toHaveLength(1);
      expect(warnings[0][1]).toContain("task-unproven");
      expect(warnings[0][1]).toContain("duplicate possible");
      await clock.fire(50);
      expect(
        sessionBgStates.get("unproven")?.pendingCompletions.map((item) => item.task_id),
      ).toEqual(["task-unproven"]);
      expect(sessionBgStates.get("unproven")?.deliveringTaskIds.size).toBe(0);
      expect(send.mock.calls.filter((call) => call[0] === "bash_ack_completions")).toHaveLength(0);

      for (let activity = 0; activity < 3; activity++) {
        await handleSubcBgEventsNudge(drainCtx);
        observeOpenCodeBgNotificationEvent({
          type: "session.idle",
          properties: { sessionID: "unproven" },
        });
        await clock.flush();
        await clock.fireIfPresent(200);
      }
      await clock.fireIfPresent(120_000);
      await clock.fireIfPresent(2000);
      expect(promptAsync).toHaveBeenCalledTimes(2);
      const nextTurn = { output: "" };
      await appendPendingWakeRecovery(drainCtx, nextTurn);
      expect(nextTurn.output).toContain("task-unproven");
      expect(sessionBgStates.get("unproven")?.pendingCompletions).toHaveLength(0);
      expect(sessionBgStates.get("unproven")?.pendingWakeAdmissions.size).toBe(0);
      expect(send.mock.calls.filter((call) => call[0] === "bash_ack_completions")).toHaveLength(1);
      expect(promptAsync).toHaveBeenCalledTimes(2);
    } finally {
      __resetBgNotificationStateForTests();
      clock.restore();
    }
  });

  test("unknown V1 wake accepts only exact stored synthetic text and never replays after readback", async () => {
    const clock = controlledWakeTimers();
    __setBgNotificationHopTimeoutForTests(50);
    let snapshot: unknown = { error: { name: "NotFoundError" }, response: { status: 404 } };
    const promptAsync = mock(
      (input: unknown) =>
        new Promise((_, reject) => {
          (input as { signal?: AbortSignal }).signal?.addEventListener(
            "abort",
            () => reject(new DOMException("aborted", "AbortError")),
            { once: true },
          );
        }),
    );
    const message = mock(async () => snapshot);
    const client = { session: { promptAsync, message, messages: async () => ({ data: [] }) } };
    const send = mock(async (command: string) =>
      command === "bash_drain_completions"
        ? { success: true, bg_completions: [completion("task-readback", "npm test")] }
        : { success: true, acked_task_ids: ["task-readback"] },
    );
    const { ctx } = harness(send);
    const drainCtx = { ctx, directory: "/project", sessionID: "readback", client };
    try {
      await handleSubcBgEventsNudge(drainCtx);
      await clock.fire(200);
      const posted = promptAsync.mock.calls[0][0] as {
        body: { messageID: string; parts: Array<{ text: string; synthetic: boolean }> };
      };
      await clock.fire(50);
      const info = { role: "user", id: posted.body.messageID, sessionID: "readback" };
      for (const bad of [
        { info: { ...info, sessionID: "other" }, parts: posted.body.parts },
        { info, parts: [{ ...posted.body.parts[0], text: "different text" }] },
        { info, parts: [{ ...posted.body.parts[0], ignored: true }] },
        { info, parts: [] },
      ]) {
        snapshot = { data: bad };
        await clock.fire(2000);
        expect(promptAsync).toHaveBeenCalledTimes(1);
        expect(send.mock.calls.filter((call) => call[0] === "bash_ack_completions")).toHaveLength(
          0,
        );
      }
      snapshot = { data: { info, parts: posted.body.parts } };
      await clock.fire(2000);
      expect(send.mock.calls.filter((call) => call[0] === "bash_ack_completions")).toHaveLength(1);
      expect(sessionBgStates.get("readback")?.pendingWakeAdmissions.size).toBe(0);
      await clock.fireIfPresent(120_000);
      expect(promptAsync).toHaveBeenCalledTimes(1);
    } finally {
      __resetBgNotificationStateForTests();
      clock.restore();
    }
  });

  test("matching assistant child reconciles an exhausted V1 wake without a third prompt", async () => {
    const clock = controlledWakeTimers();
    __setBgNotificationHopTimeoutForTests(50);
    const promptAsync = mock(() => new Promise(() => {}));
    const client = makeClient(promptAsync, []);
    const send = mock(async (command: string) =>
      command === "bash_drain_completions"
        ? { success: true, bg_completions: [completion("task-child", "npm test")] }
        : { success: true, acked_task_ids: ["task-child"] },
    );
    const { ctx } = harness(send);
    const drainCtx = { ctx, directory: "/project", sessionID: "child", client };
    try {
      await handleSubcBgEventsNudge(drainCtx);
      await clock.fire(200);
      const posted = promptAsync.mock.calls[0][0] as { body: { messageID: string } };
      await clock.fire(50);
      await clock.fire(120_000);
      await clock.fire(50);
      expect(sessionBgStates.get("child")?.pendingCompletions).toHaveLength(1);
      observeOpenCodeBgNotificationEvent({
        type: "message.updated",
        properties: {
          info: {
            role: "assistant",
            sessionID: "child",
            parentID: "unrelated",
            id: "assistant-unrelated",
          },
        },
      });
      await clock.flush();
      expect(send.mock.calls.filter((call) => call[0] === "bash_ack_completions")).toHaveLength(0);
      observeOpenCodeBgNotificationEvent({
        type: "message.updated",
        properties: {
          info: {
            role: "assistant",
            sessionID: "child",
            parentID: posted.body.messageID,
            id: "assistant-child",
          },
        },
      });
      await clock.flush();
      expect(sessionBgStates.get("child")?.pendingCompletions).toHaveLength(0);
      expect(send.mock.calls.filter((call) => call[0] === "bash_ack_completions")).toHaveLength(1);
      const output = { output: "next reply" };
      await appendInTurnBgCompletions(drainCtx, output);
      expect(output.output).toBe("next reply");
      expect(promptAsync).toHaveBeenCalledTimes(2);
    } finally {
      __resetBgNotificationStateForTests();
      clock.restore();
    }
  });

  test("disposing one V1 wake owner cancels its recovery but preserves other owners and unacked work", async () => {
    const clock = controlledWakeTimers();
    __setBgNotificationHopTimeoutForTests(50);
    const send = mock(async (command: string, params: Record<string, unknown>) =>
      command === "bash_drain_completions"
        ? { success: true, bg_completions: [completion(`task-${params.session_id}`, "npm test")] }
        : { success: true },
    );
    const { ctx: first } = harness(send);
    const { ctx: second } = harness(send);
    const promptA = mock(() => new Promise(() => {}));
    const promptB = mock(() => new Promise(() => {}));
    const a = {
      ctx: first,
      directory: "/project",
      sessionID: "owner-a",
      client: makeClient(promptA, []),
    };
    const b = {
      ctx: second,
      directory: "/project",
      sessionID: "owner-b",
      client: makeClient(promptB, []),
    };
    try {
      await handleSubcBgEventsNudge(a);
      await handleSubcBgEventsNudge(b);
      await clock.fire(200);
      await clock.fire(200);
      cancelPendingWakesForContext(first);
      await clock.flush();
      expect(
        sessionBgStates.get("owner-a")?.pendingCompletions.map((item) => item.task_id),
      ).toEqual(["task-owner-a"]);
      expect(sessionBgStates.get("owner-a")?.pendingWakeAdmissions.size).toBe(0);
      expect(sessionBgStates.get("owner-b")?.pendingWakeAdmissions.size).toBe(1);
      await clock.fire(50);
      await clock.fire(120_000);
      expect(promptA).toHaveBeenCalledTimes(1);
      expect(promptB).toHaveBeenCalledTimes(2);
      const reply = { output: "next reply" };
      await appendInTurnBgCompletions(a, reply);
      expect(reply.output).toContain("task-owner-a");
      expect(
        send.mock.calls.filter(
          (call) => call[0] === "bash_ack_completions" && call[1].session_id === "owner-a",
        ),
      ).toHaveLength(1);
    } finally {
      __resetBgNotificationStateForTests();
      clock.restore();
    }
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
    const promptAsync = mock(async () => {});

    await handleSubcBgEventsNudge({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: makeClient(promptAsync),
    });
    await waitForMockCallCount(promptAsync, 1);
    await waitUntil(
      () => send.mock.calls.filter((call) => call[0] === "bash_ack_completions").length === 1,
    );

    const prompt = promptAsync.mock.calls[0][0] as { body: { parts: Array<{ text: string }> } };
    expect(prompt.body.parts[0].text).toContain('task task-pattern matched "READY"');
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
    const promptAsync = mock(async () => {});

    await handleSubcBgEventsNudge({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: makeClient(promptAsync),
    });
    await waitForMockCallCount(promptAsync, 1);
    await waitUntil(
      () => send.mock.calls.filter((call) => call[0] === "bash_ack_completions").length === 1,
    );

    const prompt = promptAsync.mock.calls[0][0] as { body: { parts: Array<{ text: string }> } };
    expect(prompt.body.parts[0].text).toContain("- task task-exit exited:");
    expect(prompt.body.parts[0].text).toContain("task task-exit exited (exit 0)");
    expect(prompt.body.parts[0].text).not.toContain("matched");
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
      client: makeClient(mock(async () => {})),
    };

    const first = handleSubcBgEventsNudge(drainCtx);
    const duplicate = handleSubcBgEventsNudge(drainCtx);

    expect(duplicate).toBe(first);
    expect(send.mock.calls.filter((call) => call[0] === "bash_drain_completions")).toHaveLength(1);
    releaseDrain();
    await Promise.all([first, duplicate]);
    expect(findTraceEvent("subc_bg_nudge_delivery")?.cause).toBe("handler-entry");
  });

  test("a forced drain DURING in-flight delivery does not double-deliver", async () => {
    // Set up: a tracked completion is pending. A wake fires and its promptAsync
    // is held open (delivery in flight). While it's in flight, a subc bg_events
    // nudge force-drains — the module still holds the completion (not yet acked),
    // so the drain returns it. The fix must NOT schedule a second delivery.
    trackBgTask("s1", "task-1");
    let releasePrompt!: () => void;
    const promptGate = new Promise<void>((r) => {
      releasePrompt = r;
    });
    const promptAsync = mock(async () => {
      await promptGate; // hold delivery open
    });
    const client = makeClient(promptAsync);
    const send = mock(async (command: string) =>
      command === "bash_drain_completions"
        ? { success: true, bg_completions: [completion("task-1", "npm test")] }
        : { success: true, acked_task_ids: ["task-1"] },
    );
    const { ctx } = harness(send);
    const drainCtx = { ctx, directory: "/tmp/project", sessionID: "s1", client };

    // First delivery: a push schedules the wake; let the debounce timer fire so
    // promptAsync is invoked and parks on the gate (delivery in flight).
    ingestBgCompletions("s1", [completion("task-1", "npm test")]);
    await handleIdleBgCompletions(drainCtx);
    await waitForMockCallCount(promptAsync, 1);

    // While the first delivery is parked, a subc nudge force-drains.
    await handleSubcBgEventsNudge(drainCtx);
    // Give any (erroneous) second wake a chance to schedule + fire.
    await sleep(300);

    // The completion was in flight, so the forced drain must NOT have queued it
    // again — exactly ONE promptAsync delivery total.
    releasePrompt();
    await sleep(50);
    expect(promptAsync.mock.calls.length).toBe(1);
  });

  test("a forced drain RE-ACKS a delivered-but-unacked completion (C-#3 close)", async () => {
    // Delivery succeeds but the FIRST ack fails. The completion stays
    // awaiting-ack; the module re-nudges. The next forced drain must re-ack it
    // (not re-deliver), so the loop terminates.
    trackBgTask("s1", "task-1");
    let ackCalls = 0;
    const send = mock(async (command: string) => {
      if (command === "bash_drain_completions") {
        return { success: true, bg_completions: [completion("task-1", "npm test")] };
      }
      // first ack fails, subsequent acks succeed
      ackCalls += 1;
      return ackCalls === 1
        ? { success: false, message: "ack transport down" }
        : { success: true, acked_task_ids: ["task-1"] };
    });
    const { ctx } = harness(send);
    const promptAsync = mock(async () => {});
    const client = makeClient(promptAsync);
    const drainCtx = { ctx, directory: "/tmp/project", sessionID: "s1", client };

    // First delivery: push → wake → promptAsync ok → ack FAILS.
    ingestBgCompletions("s1", [completion("task-1", "npm test")]);
    await handleIdleBgCompletions(drainCtx);
    await waitForMockCallCount(promptAsync, 1);
    await sleep(50);
    // Still awaiting ack (first ack failed).
    expect(sessionBgStates.get("s1")?.deliveredAwaitingAckTaskIds.has("task-1")).toBe(true);

    // Module re-nudges → forced drain returns the still-held completion.
    await handleSubcBgEventsNudge(drainCtx);
    await sleep(50);

    // It was RE-ACKED (not re-delivered): still one promptAsync, and the
    // awaiting-ack entry is now cleared (the re-ack succeeded).
    expect(promptAsync.mock.calls.length).toBe(1);
    expect(sessionBgStates.get("s1")?.deliveredAwaitingAckTaskIds.has("task-1")).toBe(false);
  });

  test("R2-T3: a permanently-unacked delivered task is never redelivered across many drains", async () => {
    // The task is delivered, but EVERY ack fails (transport down). The daemon
    // keeps returning it on each forced drain. The fix must keep re-acking (never
    // redelivering) for as long as the daemon holds it — a prior time-based TTL
    // would have evicted it and let a later drain redeliver.
    trackBgTask("s1", "task-1");
    const send = mock(async (command: string) => {
      if (command === "bash_drain_completions") {
        return { success: true, bg_completions: [completion("task-1", "npm test")] };
      }
      return { success: false, message: "ack down" }; // every ack fails
    });
    const { ctx } = harness(send);
    const promptAsync = mock(async () => {});
    const client = makeClient(promptAsync);
    const drainCtx = { ctx, directory: "/tmp/project", sessionID: "s1", client };

    ingestBgCompletions("s1", [completion("task-1", "npm test")]);
    await handleIdleBgCompletions(drainCtx);
    await waitForMockCallCount(promptAsync, 1);
    await sleep(50);

    // Many forced drains while ack stays down — the daemon keeps returning task-1.
    for (let i = 0; i < 5; i++) {
      await handleSubcBgEventsNudge(drainCtx);
      await sleep(20);
    }

    // Still exactly ONE delivery — re-acked every time, never redelivered.
    expect(promptAsync.mock.calls.length).toBe(1);
    expect(sessionBgStates.get("s1")?.deliveredAwaitingAckTaskIds.has("task-1")).toBe(true);

    // Now the daemon DROPS it (drain returns empty, e.g. it was acked out-of-band):
    // reconciliation forgets it.
    send.mockImplementation(async (command: string) =>
      command === "bash_drain_completions"
        ? { success: true, bg_completions: [] }
        : { success: true, acked_task_ids: [] },
    );
    await handleSubcBgEventsNudge(drainCtx);
    await sleep(20);
    expect(sessionBgStates.get("s1")?.deliveredAwaitingAckTaskIds.has("task-1")).toBe(false);
  });

  test("pre-admission rejection re-pends the completion (redeliverable, not stuck in-flight)", async () => {
    trackBgTask("s1", "task-1");
    const promptAsync = mock(async () => {
      return preAdmissionRejection("promptAsync rejected");
    });
    const client = makeClient(promptAsync);
    const send = mock(async () => ({ success: true, acked_task_ids: [] }));
    const { ctx } = harness(send);
    const drainCtx = { ctx, directory: "/tmp/project", sessionID: "s1", client };

    ingestBgCompletions("s1", [completion("task-1", "npm test")]);
    await handleIdleBgCompletions(drainCtx);
    await waitForMockCallCount(promptAsync, 1);
    await sleep(50);

    const state = sessionBgStates.get("s1");
    // The host rejected before scheduling work, so retrying cannot duplicate admission.
    expect(state?.deliveringTaskIds.has("task-1")).toBe(false);
    expect(state?.deliveredAwaitingAckTaskIds.has("task-1")).toBe(false);
    expect(state?.pendingCompletions.some((c) => c.task_id === "task-1")).toBe(true);
  });

  test("pre-admission model-context rejection retries the same identity without model before ack", async () => {
    const promptAsync = mock(async (input: unknown) => {
      const body = (input as { body?: { model?: unknown } }).body;
      if (body?.model) {
        return preAdmissionRejection("model context rejected before admission");
      }
    });
    const client = makeClient(promptAsync, [
      {
        info: {
          role: "assistant",
          agent: "build",
          providerID: "anthropic-legacy",
          modelID: "claude-opus-3",
          variant: "thinking",
        },
      },
    ]);
    const send = mock(async (command: string) =>
      command === "bash_drain_completions"
        ? { success: true, bg_completions: [completion("task-legacy", "npm test")] }
        : { success: true, acked_task_ids: ["task-legacy"] },
    );
    const { ctx } = harness(send);

    await handleSubcBgEventsNudge({ ctx, directory: "/tmp/project", sessionID: "s1", client });
    await waitForMockCallCount(promptAsync, 1);
    await sleep(50);

    expect(promptAsync).toHaveBeenCalledTimes(2);
    const firstIdentity = (promptAsync.mock.calls[0][0] as { body: { messageID?: string } }).body
      .messageID;
    const retryIdentity = (promptAsync.mock.calls[1][0] as { body: { messageID?: string } }).body
      .messageID;
    expect(firstIdentity).toEqual(expect.any(String));
    expect(firstIdentity).not.toBe("");
    expect(retryIdentity).toBe(firstIdentity);
    expect(
      (promptAsync.mock.calls[0][0] as { body: { model?: unknown } }).body.model,
    ).toBeDefined();
    expect(
      (promptAsync.mock.calls[1][0] as { body: { model?: unknown } }).body.model,
    ).toBeUndefined();
    expect(send.mock.calls.filter((call) => call[0] === "bash_ack_completions")).toHaveLength(1);
  });

  test("a fresh nudge re-arms a hard-stopped failed delivery without skip-and-re-ack loss", async () => {
    let rejectInjection = true;
    const promptAsync = mock(async () => {
      if (rejectInjection) {
        return preAdmissionRejection("model context rejected before admission");
      }
    });
    const client = makeClient(promptAsync);
    const send = mock(async (command: string) =>
      command === "bash_drain_completions"
        ? { success: true, bg_completions: [completion("task-stranded", "npm test")] }
        : { success: true, acked_task_ids: ["task-stranded"] },
    );
    const { ctx } = harness(send);
    const drainCtx = { ctx, directory: "/tmp/project", sessionID: "s1", client };

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
          "bash_completion_wake_prompt_async_error",
      ),
    ).toHaveLength(1);

    rejectInjection = false;
    await handleSubcBgEventsNudge(drainCtx);
    await waitForMockCallCount(promptAsync, 6);
    await waitUntil(
      () => send.mock.calls.filter((call) => call[0] === "bash_ack_completions").length === 1,
    );

    expect(sessionBgStates.get("s1")?.pendingCompletions).toHaveLength(0);
  });

  test("a hung V1 injection retries only at the bounded readback window with the same identity", async () => {
    const clock = controlledWakeTimers();
    __setBgNotificationHopTimeoutForTests(50);
    let hangInjection = true;
    const promptAsync = mock(() =>
      hangInjection ? new Promise(() => {}) : Promise.resolve({ response: { status: 204 } }),
    );
    const client = makeClient(promptAsync);
    const send = mock(async (command: string) =>
      command === "bash_drain_completions"
        ? { success: true, bg_completions: [completion("task-hung", "npm test")] }
        : { success: true, acked_task_ids: ["task-hung"] },
    );
    const { ctx } = harness(send);
    const drainCtx = { ctx, directory: "/project", sessionID: "s1", client };
    try {
      await handleSubcBgEventsNudge(drainCtx);
      await clock.fire(200);
      const original = promptAsync.mock.calls[0][0] as { body: unknown };
      await clock.fire(50);
      expect(sessionBgStates.get("s1")?.deliveringTaskIds.has("task-hung")).toBe(true);
      expect(send.mock.calls.filter((call) => call[0] === "bash_ack_completions")).toHaveLength(0);
      hangInjection = false;
      await handleSubcBgEventsNudge(drainCtx);
      expect(promptAsync).toHaveBeenCalledTimes(1);
      await clock.fire(120_000);
      expect(promptAsync).toHaveBeenCalledTimes(2);
      expect((promptAsync.mock.calls[1][0] as { body: unknown }).body).toEqual(original.body);
      expect(send.mock.calls.filter((call) => call[0] === "bash_ack_completions")).toHaveLength(1);
    } finally {
      __resetBgNotificationStateForTests();
      clock.restore();
    }
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
    const promptAsync = mock(async () => {});
    const drainCtx = {
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: makeClient(promptAsync),
    };

    const first = handleSubcBgEventsNudge(drainCtx);
    expect(handleSubcBgEventsNudge(drainCtx)).toBe(first);
    await first;

    hangDrain = false;
    await handleSubcBgEventsNudge(drainCtx);
    await waitForMockCallCount(promptAsync, 1);
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
    client: {},
    config: {},
    storageDir: "/tmp/aft-test",
  } as unknown as PluginContext;
  return { ctx };
}

function completion(task_id: string, command: string) {
  return { task_id, status: "completed", exit_code: 0, command };
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

function controlledWakeTimers() {
  type Handle = ReturnType<typeof setTimeout>;
  const active = new Map<
    Handle,
    { callback: (...args: any[]) => void; delay: number; args: any[] }
  >();
  const time = spyOn(Date, "now").mockReturnValue(1000);
  const set = spyOn(globalThis, "setTimeout").mockImplementation(((
    callback: (...args: any[]) => void,
    delay = 0,
    ...args: any[]
  ) => {
    const handle = { unref: () => handle } as unknown as Handle;
    active.set(handle, { callback, delay, args });
    return handle;
  }) as typeof setTimeout);
  const clear = spyOn(globalThis, "clearTimeout").mockImplementation((handle) => {
    active.delete(handle as Handle);
  });
  const flush = async () => {
    for (let turn = 0; turn < 30; turn++) await Promise.resolve();
  };
  const fireIfPresent = async (delay: number) => {
    const entry = [...active].find(([, timer]) => timer.delay === delay);
    if (!entry) return false;
    const [handle, timer] = entry;
    active.delete(handle);
    timer.callback(...timer.args);
    await flush();
    return true;
  };
  return {
    flush,
    fireIfPresent,
    async fire(delay: number) {
      expect(await fireIfPresent(delay)).toBe(true);
    },
    restore() {
      set.mockRestore();
      clear.mockRestore();
      time.mockRestore();
    },
  };
}

/** SDK-shaped validation failure: the host returns this before forking prompt work. */
function preAdmissionRejection(message: string) {
  return {
    error: { name: "BadRequest", data: { message, kind: "Body" } },
    response: { status: 400 },
  };
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
        'stdout byte 0 ×2: ERROR a\n3 matching lines dropped. Read them with bash_status({ taskId: "bash-lines", outputOffset: 16 }).',
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
    '3 matching lines dropped. Read them with bash_status({ taskId: "bash-lines", outputOffset: 16 }).',
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
  const drain = { ctx, directory: "/tmp/project", sessionID: "s-lines", client: {} };
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
  test(`busy each_line batches render once in order with an exact ack (${route})`, async () => {
    const sessionID = "s-busy-lines";
    const frames = [0, 18, 36].map((offset, index) => ({
      task_id: "bash-busy-lines",
      session_id: sessionID,
      watch_id: "watch-busy-lines",
      match_text: "matching lines",
      match_offset: offset,
      context: "unused: render the batch",
      once: false,
      reason: "pattern_match" as const,
      batch: {
        pending_id: "watch-busy-lines:0",
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
    const promptAsync = mock(async () => {});
    const context = { ctx, directory: "/project", sessionID, client: makeClient(promptAsync) };
    const busy = () =>
      observeOpenCodeBgNotificationEvent({
        type: "session.status",
        properties: { sessionID, status: { type: "busy" } },
      });
    busy();
    trackBgTask(sessionID, "bash-busy-lines");
    // Registration can be followed by a drain before any matching output exists.
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
        clearSessionBgState(sessionID);
        busy();
        await handleSubcBgEventsNudge(context);
      }
    }
    expect(promptAsync).not.toHaveBeenCalled();
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
    const output = { output: "foreground result" };
    await appendInTurnBgCompletions(context, output);
    expect(output.output.split("\n").filter((line) => line.includes("stdout byte"))).toEqual([
      "      > stdout byte 0: LIVECHECK3 line 1",
      "      > stdout byte 18: LIVECHECK3 line 2",
      "      > stdout byte 36: LIVECHECK3 line 3",
    ]);
    expect(acks).toEqual([
      {
        session_id: sessionID,
        matches: [{ task_id: "bash-busy-lines", watch_id: "watch-busy-lines", match_offset: 36 }],
      },
    ]);
    const next = { output: "next result" };
    await appendInTurnBgCompletions(context, next);
    expect(next.output).toBe("next result");
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
    const output = { output: "tool" };
    await appendInTurnBgCompletions(context, output);
    return output.output;
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
    const client = makeClient(
      mock(async (input: unknown) => {
        sent.push((input as { body: { parts: { text: string }[] } }).body.parts[0].text);
      }),
    );
    const context = { ctx, directory: "/tmp/project", sessionID: "s1", client };
    const deliver = async () => {
      if (route === "in-turn") {
        const output = { output: "tool" };
        await appendInTurnBgCompletions(context, output);
        return output.output;
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
