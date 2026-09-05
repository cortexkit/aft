/// <reference path="../bun-test.d.ts" />

import { afterAll, afterEach, beforeEach, describe, expect, mock, test } from "bun:test";

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
}));

afterAll(() => {
  mock.restore();
});

import {
  __resetBgNotificationStateForTests,
  __setBgNotificationHopTimeoutForTests,
  appendInTurnBgCompletions,
  consumeBgCompletion,
  formatPatternMatchReminder,
  formatSystemReminder,
  handleIdleBgCompletions,
  handlePushedBgCompletion,
  handleSubcBgEventsNudge,
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
        parts: Array<{ text: string; synthetic?: boolean; ignored?: boolean }>;
      };
    };
    expect(payload.body.noReply).toBe(false);
    expect(payload.body.parts[0].text).toContain("- task task-1 (exit 0)");
    expect(payload.body.parts[0].text).not.toContain(": npm test");
    // #129: the agent-directed wake part MUST be synthetic (model-visible,
    // not a user turn, byte-stable across OpenCode's mid-turn wrapper flip)
    // and MUST NOT be `ignored` (which would strip it from the model call).
    expect(payload.body.parts[0].synthetic).toBe(true);
    expect(payload.body.parts[0].ignored).toBeUndefined();
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

  test("failed wake keeps pending completions and retries", async () => {
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({ success: true, bg_completions: [] }));
    const promptAsync = mock(async () => {
      throw new Error("send failed");
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

  test("failed wake hard-stops after capped retries", async () => {
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({ success: true, bg_completions: [] }));
    const promptAsync = mock(async () => {
      throw new Error("send failed");
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

      now += SESSION_BG_STATE_IDLE_TTL_MS + 1;
      trackBgTask("active", "task-2");

      expect(sessionBgStates.has("stale")).toBe(false);
      expect(sessionBgStates.has("active")).toBe(true);
    } finally {
      Date.now = originalDateNow;
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
});

describe("subc forced-drain dedup (C-#1 / C-#3)", () => {
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

  test("delivery FAILURE re-pends the completion (redeliverable, not stuck in-flight)", async () => {
    trackBgTask("s1", "task-1");
    const promptAsync = mock(async () => {
      throw new Error("promptAsync rejected");
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
    // Delivery failed before departing → cleared from in-flight, back in pending.
    expect(state?.deliveringTaskIds.has("task-1")).toBe(false);
    expect(state?.deliveredAwaitingAckTaskIds.has("task-1")).toBe(false);
    expect(state?.pendingCompletions.some((c) => c.task_id === "task-1")).toBe(true);
  });

  test("legacy model rejection retries the same wake without model before ack", async () => {
    const promptAsync = mock(async (input: unknown) => {
      const body = (input as { body?: { model?: unknown } }).body;
      if (body?.model) {
        throw new Error("ProviderModelNotFoundError: migrated session model is stale");
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
        throw new Error("ProviderModelNotFoundError: migrated session model is stale");
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

  test("a hung session injection times out, clears in-flight state, and redelivers", async () => {
    __setBgNotificationHopTimeoutForTests(50);
    let hangInjection = true;
    const promptAsync = mock(() =>
      hangInjection ? new Promise<void>(() => {}) : Promise.resolve(undefined),
    );
    const client = makeClient(promptAsync);
    const send = mock(async (command: string) =>
      command === "bash_drain_completions"
        ? { success: true, bg_completions: [completion("task-hung", "npm test")] }
        : { success: true, acked_task_ids: ["task-hung"] },
    );
    const { ctx } = harness(send);
    const drainCtx = { ctx, directory: "/tmp/project", sessionID: "s1", client };

    await handleSubcBgEventsNudge(drainCtx);
    await waitForMockCallCount(promptAsync, 1);
    await waitUntil(
      () =>
        sessionBgStates.get("s1")?.deliveringTaskIds.has("task-hung") === false &&
        sessionBgStates
          .get("s1")
          ?.pendingCompletions.some((item) => item.task_id === "task-hung") === true,
      1_000,
    );

    expect(sessionBgStates.get("s1")?.deliveredAwaitingAckTaskIds.has("task-hung")).toBe(false);
    expect(send.mock.calls.filter((call) => call[0] === "bash_ack_completions")).toHaveLength(0);

    hangInjection = false;
    await handleSubcBgEventsNudge(drainCtx);
    await waitForMockCallCount(promptAsync, 2);
    await waitUntil(
      () => send.mock.calls.filter((call) => call[0] === "bash_ack_completions").length === 1,
    );
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
