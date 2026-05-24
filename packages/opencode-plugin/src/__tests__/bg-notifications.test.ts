/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, mock, test } from "bun:test";

// Spy on sessionLog/sessionWarn so we can assert on the structured trace
// events emitted by the wake path (event names, wake_client_path metadata,
// bash_completion_wake_client_unavailable). The mock MUST be installed
// before the SUT is imported, because Bun hoists `mock.module` to the
// top of the file.
const sessionLogSpy = mock(
  (_sessionID: string | undefined, _message: string, _data?: unknown) => {},
);
const sessionWarnSpy = mock(
  (_sessionID: string | undefined, _message: string, _data?: unknown) => {},
);
mock.module("../logger.js", () => ({
  sessionLog: sessionLogSpy,
  sessionWarn: sessionWarnSpy,
  log: () => {},
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

// Mock the live-server client factory + wake-availability decision so
// unit tests don't need a real HTTP listener. Each test sets up its own
// state:
//   • `setTestLiveServerClient(client)` — install the client returned by
//     `getLiveServerClient()` when the wake path picks the live-server
//     transport.
//   • `setTestLiveServerAvailable(true|false)` — flip the per-process
//     wake-availability decision the wake path reads at fire time.
//
// When availability is `false` the wake path uses `drainContext.client`
// directly (the in-process fallback), bypassing this factory entirely.
// That's the post-v0.29 behavior introduced when we removed the
// `--port 0` nudge — see shared/live-server-client.ts.
let liveServerClient: unknown = null;
let lastLiveServerArgs: { serverUrl: string; directory: string } | null = null;
let liveServerAvailable = true;
function setTestLiveServerClient(client: unknown): void {
  liveServerClient = client;
}
function setTestLiveServerAvailable(available: boolean): void {
  liveServerAvailable = available;
}
function getLastLiveServerArgs(): { serverUrl: string; directory: string } | null {
  return lastLiveServerArgs;
}
mock.module("../shared/live-server-client.js", () => ({
  getLiveServerClient: (serverUrl: string, directory: string) => {
    lastLiveServerArgs = { serverUrl, directory };
    if (!liveServerClient) {
      throw new Error("test did not configure a live-server client via setTestLiveServerClient()");
    }
    return liveServerClient;
  },
  useLiveServerWake: () => liveServerAvailable,
  setLiveServerWakeAvailable: (available: boolean) => {
    liveServerAvailable = available;
  },
  __resetLiveServerClientCacheForTests: () => {
    liveServerClient = null;
    lastLiveServerArgs = null;
  },
  __resetLiveServerWakeForTests: () => {
    liveServerAvailable = true;
  },
}));

import {
  __resetBgNotificationStateForTests,
  appendInTurnBgCompletions,
  formatSystemReminder,
  handleIdleBgCompletions,
  handlePushedBgCompletion,
  ingestBgCompletions,
  SESSION_BG_STATE_IDLE_TTL_MS,
  sessionBgStates,
  trackBgTask,
} from "../bg-notifications.js";
import type { PluginContext } from "../types.js";

type BridgeResponse = Record<string, unknown>;

const TEST_SERVER_URL = "http://127.0.0.1:0/";

beforeEach(() => {
  sessionLogSpy.mockClear();
  sessionWarnSpy.mockClear();
  liveServerClient = null;
  lastLiveServerArgs = null;
  // Default to live-server-available so existing tests keep exercising
  // the workaround path. Tests covering the fallback flip this to false.
  liveServerAvailable = true;
});

afterEach(() => {
  __resetBgNotificationStateForTests();
});

/**
 * Configure the live-server client mock to return `{ session: { promptAsync } }`,
 * optionally with a `messages` stub so prompt-context resolution works.
 */
function installLiveServerClient(
  promptAsync: (input: unknown) => Promise<unknown> | unknown,
  messages?: unknown[],
): void {
  setTestLiveServerClient({
    session: {
      promptAsync,
      ...(messages !== undefined ? { messages: async () => ({ data: messages }) } : {}),
    },
  });
}

/**
 * Build a stub plugin-context client shaped like OpenCode's `input.client`.
 * Returned so individual tests can read `.session.promptAsync.mock.calls`
 * to assert whether the in-process wake fallback fired.
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

  test("in-turn delivery drains and appends reminder to tool output", async () => {
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({
      success: true,
      bg_completions: [completion("task-1", "echo done")],
    }));
    const output = { output: "tool output" };

    // In-turn delivery never calls promptAsync, so no live-server client
    // setup is needed.
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
    installLiveServerClient(promptAsync);

    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: {},
      serverUrl: TEST_SERVER_URL,
    });
    await waitForMockCallCount(promptAsync, 1);

    expect(promptAsync).toHaveBeenCalledTimes(1);
    const payload = promptAsync.mock.calls[0][0] as {
      body: { noReply: boolean; parts: Array<{ text: string }> };
    };
    expect(payload.body.noReply).toBe(false);
    expect(payload.body.parts[0].text).toContain("- task task-1 (exit 0)");
    expect(payload.body.parts[0].text).not.toContain(": npm test");
    // Live-server factory was called with the URL + directory we provided.
    expect(getLastLiveServerArgs()).toEqual({
      serverUrl: TEST_SERVER_URL,
      directory: "/tmp/project",
    });
  });

  test("turn-end wake forwards resolved agent + model + variant to preserve prefix cache", async () => {
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({
      success: true,
      bg_completions: [completion("task-1", "npm test")],
    }));
    const promptAsync = mock(async () => {});
    installLiveServerClient(promptAsync, [
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
      client: {},
      serverUrl: TEST_SERVER_URL,
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
    installLiveServerClient(promptAsync, []);

    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: {},
      serverUrl: TEST_SERVER_URL,
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

  test("push completion lands in pending and wakes when idle", async () => {
    trackBgTask("s1", "task-1");
    const send = mock(async () => ({
      success: true,
      bg_completions: [],
      acked_task_ids: ["task-1"],
    }));
    const { ctx } = harness(send);
    const promptAsync = mock(async () => {});
    installLiveServerClient(promptAsync);

    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client: {},
        serverUrl: TEST_SERVER_URL,
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

  test("buffers push completion received before task tracking", async () => {
    const { ctx } = harness(() => ({ success: true, bg_completions: [] }));
    const promptAsync = mock(async () => {});
    installLiveServerClient(promptAsync);

    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client: {},
        serverUrl: TEST_SERVER_URL,
      },
      completion("task-1", "npm test"),
    );
    trackBgTask("s1", "task-1");
    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: {},
      serverUrl: TEST_SERVER_URL,
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
    installLiveServerClient(promptAsync);

    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client: {},
        serverUrl: TEST_SERVER_URL,
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
    installLiveServerClient(promptAsync);

    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client: {},
        serverUrl: TEST_SERVER_URL,
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

  test("push completion still wakes even when bridge is busy with non-agent RPC", async () => {
    // Regression: previously bailed on `isActive()` (bridge.hasPendingRequests())
    // which returned true for the TUI status poll, orphaning the completion when
    // no other trigger fired. The wake must always be scheduled.
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({ success: true, bg_completions: [] }));
    const promptAsync = mock(async () => {});
    installLiveServerClient(promptAsync);

    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client: {},
        serverUrl: TEST_SERVER_URL,
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
    installLiveServerClient(promptAsync);

    for (const taskId of ["task-1", "task-2", "task-3"]) trackBgTask("s1", taskId);
    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: {},
      serverUrl: TEST_SERVER_URL,
    });
    await sleep(50);
    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: {},
      serverUrl: TEST_SERVER_URL,
    });
    await sleep(50);
    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: {},
      serverUrl: TEST_SERVER_URL,
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
    installLiveServerClient(promptAsync);
    const started = Date.now();

    for (let task = 1; task <= 6; task++) trackBgTask("s1", `task-${task}`);
    for (let tick = 0; tick < 6; tick++) {
      await handleIdleBgCompletions({
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client: {},
        serverUrl: TEST_SERVER_URL,
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
    installLiveServerClient(promptAsync);
    let responses: BridgeResponse[] = [
      { success: true, bg_completions: [completion("task-1", "one")] },
    ];
    const { ctx } = harness(() => responses.shift() ?? { success: true, bg_completions: [] });

    trackBgTask("s1", "task-1");
    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: {},
      serverUrl: TEST_SERVER_URL,
    });
    await waitForMockCallCount(promptAsync, 1);
    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: {},
      serverUrl: TEST_SERVER_URL,
    });
    expect(sessionBgStates.get("s1")?.debounceTimer ?? null).toBeNull();
    expect(promptAsync).toHaveBeenCalledTimes(1);

    responses = [{ success: true, bg_completions: [completion("task-2", "two")] }];
    trackBgTask("s1", "task-2");
    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: {},
      serverUrl: TEST_SERVER_URL,
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

  // ─── Wake transport selection (live-server vs. in-process fallback) ───
  //
  // Per-process decision is made by `setLiveServerWakeAvailable()` at
  // plugin init from the result of `probeServerReachable()`. The wake
  // path reads the cached decision via `useLiveServerWake()` each time
  // a reminder fires.
  //
  // • `true`  — POST through `createOpencodeClient(input.serverUrl)`.
  //             Works around anomalyco/opencode#28202 (no duplicate runs).
  // • `false` — POST through `drainContext.client.session.promptAsync`.
  //             Accepts the upstream bug so wakes still arrive instead
  //             of being indefinitely queued + dropped via wake_hard_stop.

  test("live-server wake uses createOpencodeClient and tags trace as live-server", async () => {
    setTestLiveServerAvailable(true);
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({
      success: true,
      bg_completions: [completion("task-1", "npm test")],
    }));
    const livePromptAsync = mock(async () => {});
    installLiveServerClient(livePromptAsync);
    const fallbackClient = makeClient(mock(async () => {}));

    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: fallbackClient,
      serverUrl: TEST_SERVER_URL,
    });
    await waitForMockCallCount(livePromptAsync, 1);

    // The live-server client was used; the fallback client was NOT.
    expect(livePromptAsync).toHaveBeenCalledTimes(1);
    expect(fallbackClient.session.promptAsync).toHaveBeenCalledTimes(0);

    const startMeta = findTraceEvent("bash_completion_wake_prompt_async_start");
    expect(startMeta).toBeDefined();
    expect(startMeta?.wake_client_path).toBe("live-server");
    expect(typeof startMeta?.delivery_id).toBe("string");
    expect(startMeta?.task_ids).toEqual(["task-1"]);
    // The factory saw the serverUrl + directory we configured.
    expect(getLastLiveServerArgs()).toEqual({
      serverUrl: TEST_SERVER_URL,
      directory: "/tmp/project",
    });
  });

  test("in-process fallback wake uses drainContext.client and tags trace accordingly", async () => {
    // When the live HTTP listener was unreachable at startup,
    // bg-notifications must use the plugin-provided in-process client so
    // wakes still arrive — at the cost of the upstream duplicate-runner
    // bug. Pre-v0.29 we threw and queued for retry; post-v0.29 we
    // intentionally accept the bug in exchange for delivery.
    setTestLiveServerAvailable(false);
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({
      success: true,
      bg_completions: [completion("task-1", "npm test")],
    }));
    const livePromptAsync = mock(async () => {});
    installLiveServerClient(livePromptAsync);
    const fallbackClient = makeClient(mock(async () => {}));

    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: fallbackClient,
      serverUrl: TEST_SERVER_URL,
    });
    await waitForMockCallCount(fallbackClient.session.promptAsync, 1);

    // The fallback client was used; the live-server factory was NOT
    // consulted at all (no probe of getLastLiveServerArgs).
    expect(fallbackClient.session.promptAsync).toHaveBeenCalledTimes(1);
    expect(livePromptAsync).toHaveBeenCalledTimes(0);
    expect(getLastLiveServerArgs()).toBeNull();

    const startMeta = findTraceEvent("bash_completion_wake_prompt_async_start");
    expect(startMeta).toBeDefined();
    expect(startMeta?.wake_client_path).toBe("in-process-fallback");
    expect(typeof startMeta?.delivery_id).toBe("string");
    expect(startMeta?.task_ids).toEqual(["task-1"]);
  });

  test("in-process fallback without client emits diagnostic and queues for retry", async () => {
    // If the live-server probe said false AND the drainContext somehow
    // arrived without a client, the wake has no transport at all. The
    // path emits a dedicated trace event, holds completions for retry,
    // and lets the existing retry-with-backoff fire — same behavior the
    // pre-v0.29 missing-serverUrl path used to have.
    setTestLiveServerAvailable(false);
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({ success: true, bg_completions: [] }));
    const livePromptAsync = mock(async () => {});
    installLiveServerClient(livePromptAsync);

    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        // client intentionally omitted
        serverUrl: TEST_SERVER_URL,
      },
      completion("task-1", "npm test"),
    );
    await waitUntil(() => findTraceEvent("bash_completion_wake_client_unavailable") !== undefined);

    // No client = no transport = no promptAsync call on either path.
    expect(livePromptAsync).toHaveBeenCalledTimes(0);
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
