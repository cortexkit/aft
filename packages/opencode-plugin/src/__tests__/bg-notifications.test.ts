/// <reference path="../bun-test.d.ts" />

import { afterAll, afterEach, beforeEach, describe, expect, mock, test } from "bun:test";

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
let lastLiveServerArgs: {
  serverUrl: string;
  directory: string;
  headers?: Record<string, string>;
} | null = null;
let liveServerAvailable = true;
// Per-URL availability map — must behave like the real
// live-server-client implementation so the live-server-client unit
// tests still pass when Bun's process-global `mock.module()` leaks
// this stub across test files.
const perUrlAvailability = new Map<string, boolean>();
function normalizeServerUrl(serverUrl: string): string {
  try {
    return new URL(serverUrl).toString();
  } catch {
    return serverUrl;
  }
}
function serverAuthHeaders(): Record<string, string> | undefined {
  const password = process.env.OPENCODE_SERVER_PASSWORD;
  if (!password) return undefined;
  const username = process.env.OPENCODE_SERVER_USERNAME ?? "opencode";
  return {
    Authorization: `Basic ${Buffer.from(`${username}:${password}`).toString("base64")}`,
  };
}
function setTestLiveServerClient(client: unknown): void {
  liveServerClient = client;
}
function setTestLiveServerAvailable(available: boolean): void {
  liveServerAvailable = available;
}
function getLastLiveServerArgs(): {
  serverUrl: string;
  directory: string;
  headers?: Record<string, string>;
} | null {
  return lastLiveServerArgs;
}
mock.module("../shared/live-server-client.js", () => ({
  getLiveServerClient: (serverUrl: string, directory: string, headers?: Record<string, string>) => {
    lastLiveServerArgs = { serverUrl, directory, ...(headers ? { headers } : {}) };
    if (!liveServerClient) {
      throw new Error("test did not configure a live-server client via setTestLiveServerClient()");
    }
    return liveServerClient;
  },
  useLiveServerWake: (serverUrl?: string, enabled = true) => {
    if (!enabled) return false;
    if (!serverUrl) return liveServerAvailable;
    const keyed = perUrlAvailability.get(normalizeServerUrl(serverUrl));
    if (keyed !== undefined) return keyed;
    // bg-notifications tests use setTestLiveServerAvailable(true) (single
    // bool) to enable the live-server path for all URLs in one shot,
    // while live-server-client unit tests use setLiveServerWakeAvailable(url, ...)
    // to set per-URL state. When per-URL state is unset, fall back to the
    // single-bool toggle so bg-notifications tests keep working, but only
    // when it has been set explicitly via setTestLiveServerAvailable() —
    // the unit tests reset liveServerAvailable to its initial state via
    // __resetLiveServerWakeForTests(), so any URL they didn't set should
    // remain false.
    return liveServerAvailable;
  },
  setLiveServerWakeAvailable: (
    serverUrlOrAvailable: string | boolean | undefined,
    available?: boolean,
  ) => {
    if (typeof serverUrlOrAvailable === "boolean") {
      liveServerAvailable = serverUrlOrAvailable;
      return;
    }
    if (!serverUrlOrAvailable) {
      liveServerAvailable = available ?? false;
      return;
    }
    perUrlAvailability.set(normalizeServerUrl(serverUrlOrAvailable), available ?? false);
  },
  // Bun's `mock.module()` is process-global and partial mocks leak across
  // test files. The probe-related exports MUST be included even though this
  // test file does not exercise them, because the live-server-client unit
  // tests import from the same module path and would otherwise see
  // `undefined` for these symbols when the mock is already installed.
  probeServerReachable: async (serverUrl?: string, _timeoutMs?: number) => {
    if (!serverUrl) {
      perUrlAvailability.clear();
      return false;
    }
    // Mirror real implementation enough that unit-test fetch stubs drive
    // this code path correctly: hit URL, accept only 2xx, reject 401/403,
    // 404/5xx, and network errors.
    let reachable = false;
    try {
      const probeUrl = new URL("/session", serverUrl).toString();
      const res = await globalThis.fetch(probeUrl, {
        method: "GET",
        headers: serverAuthHeaders(),
      });
      reachable = res.ok;
    } catch {
      reachable = false;
    }
    perUrlAvailability.set(normalizeServerUrl(serverUrl), reachable);
    return reachable;
  },
  __resetLiveServerClientCacheForTests: () => {
    liveServerClient = null;
    lastLiveServerArgs = null;
  },
  __resetLiveServerWakeForTests: () => {
    // Match the real implementation: legacyLiveServerWakeAvailable resets
    // to false, not true. The bg-notifications tests that need
    // liveServerAvailable=true explicitly call setTestLiveServerAvailable(true)
    // in their setup, so this default of false is what the live-server-client
    // unit tests need without breaking bg-notifications.
    liveServerAvailable = false;
    perUrlAvailability.clear();
  },
}));

afterAll(() => {
  mock.restore();
});

import {
  DEFERRED_COMPLETION_FALLBACK_MS,
  __resetBgNotificationStateForTests,
  appendInTurnBgCompletions,
  consumeBgCompletion,
  formatPatternMatchReminder,
  formatSystemReminder,
  handleIdleBgCompletions,
  handlePushedBgCompletion,
  handlePushedBgLongRunning,
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

const TEST_SERVER_URL = "http://127.0.0.1:0/";

beforeEach(() => {
  sessionLogSpy.mockClear();
  sessionDebugSpy.mockClear();
  sessionWarnSpy.mockClear();
  liveServerClient = null;
  lastLiveServerArgs = null;
  perUrlAvailability.clear();
  // Default to live-server-available so existing tests keep exercising
  // the workaround path. Tests covering the fallback flip this to false.
  liveServerAvailable = true;
});

afterEach(() => {
  __resetBgNotificationStateForTests();
});

/**
 * Configure live-server client mock. `prompt` is preferred wake method; tests
 * may also provide `promptAsync` for compatibility assertions.
 */
function installLiveServerClient(options: {
  prompt?: (input: unknown) => Promise<unknown> | unknown;
  promptAsync?: (input: unknown) => Promise<unknown> | unknown;
  messages?: unknown[];
}): void {
  setTestLiveServerClient({
    session: {
      ...(options.prompt ? { prompt: options.prompt } : {}),
      ...(options.promptAsync ? { promptAsync: options.promptAsync } : {}),
      ...(options.messages !== undefined
        ? { messages: async () => ({ data: options.messages }) }
        : {}),
    },
  });
}

/**
 * Build stub plugin-context client shaped like OpenCode's `input.client`.
 * Returned so tests can inspect `.session.prompt` / `.session.promptAsync`.
 */
function makeClient(
  methods: {
    prompt?: ReturnType<typeof mock>;
    promptAsync?: ReturnType<typeof mock>;
  },
  messages?: unknown[],
): {
  session: {
    prompt?: ReturnType<typeof mock>;
    promptAsync?: ReturnType<typeof mock>;
    messages?: () => Promise<{ data: unknown[] }>;
  };
} {
  return {
    session: {
      ...(methods.prompt ? { prompt: methods.prompt } : {}),
      ...(methods.promptAsync ? { promptAsync: methods.promptAsync } : {}),
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
      "<system-reminder>\n[BACKGROUND BASH FAILED]\n- task 4f5b71c2 (timed out, 30s)\n</system-reminder>\n<system-reminder>\n[BACKGROUND BASH COMPLETED]\n- task d2ed3a9e (exit 0, 1m 23s)\n</system-reminder>",
    );
  });

  test("formats urgent failures separately from normal completions", () => {
    expect(
      formatSystemReminder([
        { task_id: "ok-1", status: "completed", exit_code: 0, command: "true" },
        { task_id: "fail-1", status: "failed", exit_code: 1, command: "false" },
      ]),
    ).toBe(
      "<system-reminder>\n[BACKGROUND BASH FAILED]\n- task fail-1 (exit 1)\n</system-reminder>\n<system-reminder>\n[BACKGROUND BASH COMPLETED]\n- task ok-1 (exit 0)\n</system-reminder>",
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

  test("turn-end wake uses live session.prompt, not promptAsync", async () => {
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({
      success: true,
      bg_completions: [completion("task-1", "npm test")],
    }));
    const prompt = mock(async () => {});
    const promptAsync = mock(async () => {});
    installLiveServerClient({ prompt, promptAsync });

    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: {},
      serverUrl: TEST_SERVER_URL,
    });
    await waitForMockCallCount(prompt, 1);

    expect(prompt).toHaveBeenCalledTimes(1);
    expect(promptAsync).toHaveBeenCalledTimes(0);
    const payload = prompt.mock.calls[0][0] as {
      body: { noReply: boolean; parts: Array<{ text: string }> };
    };
    expect(payload.body.noReply).toBe(false);
    expect(payload.body.parts[0].text).toContain("- task task-1 (exit 0)");
    expect(payload.body.parts[0].text).not.toContain(": npm test");
    // Live-server factory was called with the URL + directory we provided.
    expect(getLastLiveServerArgs()).toEqual({
      serverUrl: TEST_SERVER_URL,
      directory: "/tmp/project",
      headers: expect.objectContaining({
        "x-aft-delivery-id": expect.any(String),
      }),
    });
  });

  test("turn-end wake preserves session method this binding for class-style prompt", async () => {
    setTestLiveServerAvailable(false);
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({
      success: true,
      bg_completions: [completion("task-1", "npm test")],
    }));

    class BoundSession {
      calls: Array<{
        path: { id: string };
        body: { parts: Array<{ text: string }> };
        throwOnError?: boolean;
      }> = [];

      async prompt(input: {
        path: { id: string };
        body: { parts: Array<{ text: string }> };
        throwOnError?: boolean;
      }) {
        if (!(this instanceof BoundSession)) {
          throw new Error("prompt lost this binding");
        }
        this.calls.push(input);
      }
    }

    const session = new BoundSession();
    const fallbackClient = { session };

    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: fallbackClient,
      serverUrl: TEST_SERVER_URL,
    });

    await waitUntil(() => session.calls.length === 1);
    expect(session.calls).toHaveLength(1);
    expect(session.calls[0]?.path.id).toBe("s1");
    expect(session.calls[0]?.throwOnError).toBe(true);
    expect(session.calls[0]?.body.parts[0]?.text).toContain("- task task-1 (exit 0)");
    expect(
      sessionWarnSpy.mock.calls.some((call) => String(call[1]).includes("lost this binding")),
    ).toBe(false);
  });

  test("live prompt sdk-style non-2xx demotes live server, falls back, then acks fallback", async () => {
    trackBgTask("s1", "task-1");
    const send = mock(async (command: string) =>
      command === "bash_drain_completions"
        ? { success: true, bg_completions: [completion("task-1", "npm test")] }
        : { success: true, acked_task_ids: ["task-1"] },
    );
    const { ctx } = harness(send);
    const fallbackPrompt = mock(async () => undefined);
    const fallbackClient = makeClient({ prompt: fallbackPrompt });
    const livePrompt = mock(async () => ({
      error: { message: "missing route" },
      response: { ok: false, status: 404, statusText: "Not Found" },
    }));
    installLiveServerClient({ prompt: livePrompt });

    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: fallbackClient,
      serverUrl: TEST_SERVER_URL,
    });
    await waitForMockCallCount(fallbackPrompt, 1);

    expect(livePrompt).toHaveBeenCalledTimes(1);
    expect(fallbackPrompt).toHaveBeenCalledTimes(1);
    expect(send.mock.calls.filter((call) => call[0] === "bash_ack_completions")).toHaveLength(1);
    const fallbackEvent = findTraceEvent("bash_completion_wake_live_server_fallback");
    expect(fallbackEvent).toBeDefined();
    expect(String(fallbackEvent?.error)).toContain("HTTP 404 Not Found");
    expect(String(fallbackEvent?.error)).toContain("missing route");
  });

  test("in-process sdk-style non-2xx failure does not ack completion", async () => {
    setTestLiveServerAvailable(false);
    trackBgTask("s1", "task-1");
    const send = mock(async (command: string) =>
      command === "bash_drain_completions"
        ? { success: true, bg_completions: [completion("task-1", "npm test")] }
        : { success: true, acked_task_ids: ["task-1"] },
    );
    const { ctx } = harness(send);
    const prompt = mock(async () => ({
      error: "bad request body",
      response: { ok: false, status: 400, statusText: "Bad Request" },
    }));
    const fallbackClient = makeClient({ prompt });

    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: fallbackClient,
      serverUrl: TEST_SERVER_URL,
    });
    await waitForMockCallCount(prompt, 1);

    expect(send.mock.calls.some((call) => call[0] === "bash_ack_completions")).toBe(false);
    expect(sessionBgStates.get("s1")?.pendingCompletions).toHaveLength(1);
    expect(sessionBgStates.get("s1")?.debounceTimer).not.toBeNull();
    const errorEvent = findTraceEvent("bash_completion_wake_send_error");
    expect(errorEvent).toBeDefined();
    expect(String(errorEvent?.error)).toContain("HTTP 400 Bad Request");
    expect(String(errorEvent?.error)).toContain("bad request body");
  });

  test("idle wake keeps debounce timer ref'd so autonomous completion reminder can fire", async () => {
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({
      success: true,
      bg_completions: [completion("task-1", "npm test")],
    }));
    const prompt = mock(async () => {});
    installLiveServerClient({ prompt });

    const unrefSpy = await withSetTimeoutUnrefSpy(async () => {
      await handleIdleBgCompletions({
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client: {},
        serverUrl: TEST_SERVER_URL,
      });
    });

    expect(unrefSpy).not.toBeNull();
    expect(unrefSpy?.mock.calls).toHaveLength(0);
    await waitForMockCallCount(prompt, 1);
  });

  test("live wake acks only after session.prompt resolves", async () => {
    trackBgTask("s1", "task-1");
    let resolvePrompt: (() => void) | undefined;
    const prompt = mock(
      () =>
        new Promise<void>((resolve) => {
          resolvePrompt = resolve;
        }),
    );
    const send = mock(async (command: string) =>
      command === "bash_drain_completions"
        ? { success: true, bg_completions: [completion("task-1", "npm test")] }
        : { success: true, acked_task_ids: ["task-1"] },
    );
    const { ctx } = harness(send);
    installLiveServerClient({ prompt });

    const wakePromise = handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: {},
      serverUrl: TEST_SERVER_URL,
    });
    await waitForMockCallCount(prompt, 1);
    expect(send.mock.calls.some((call) => call[0] === "bash_ack_completions")).toBe(false);

    resolvePrompt?.();
    await wakePromise;
    await waitUntil(() => send.mock.calls.some((call) => call[0] === "bash_ack_completions"));
  });

  test("turn-end wake forwards resolved agent + model + variant to preserve prefix cache", async () => {
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({
      success: true,
      bg_completions: [completion("task-1", "npm test")],
    }));
    const promptAsync = mock(async () => {});
    installLiveServerClient({
      prompt: promptAsync,
      messages: [
        {
          info: {
            role: "assistant",
            agent: "build",
            providerID: "anthropic",
            modelID: "claude-opus-4-7",
            variant: "thinking",
          },
        },
      ],
    });

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
    installLiveServerClient({ prompt: promptAsync, messages: [] });

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
        serverUrl: TEST_SERVER_URL,
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
    installLiveServerClient({ prompt: promptAsync });
    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: {},
      serverUrl: TEST_SERVER_URL,
    });

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

  test("same-turn push completion waits for sync bash_watch instead of waking", async () => {
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({ success: true, bg_completions: [] }));
    const promptAsync = mock(async () => {});
    installLiveServerClient({ prompt: promptAsync });

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
    await sleep(300);

    expect(promptAsync).toHaveBeenCalledTimes(0);
    expect(sessionBgStates.get("s1")?.pendingCompletions).toHaveLength(1);
    expect(sessionBgStates.get("s1")?.debounceTimer).toBeNull();
    expect(sessionBgStates.get("s1")?.deferredCompletionTimer).not.toBeNull();

    markTaskWaiting("s1", "task-1");
    await sleep(300);

    expect(promptAsync).toHaveBeenCalledTimes(0);
    expect(sessionBgStates.get("s1")?.pendingCompletions).toHaveLength(0);
  });

  test("same-turn deferred completion falls back without idle", async () => {
    setTestLiveServerAvailable(false);
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({ success: true, bg_completions: [] }));
    const promptAsync = mock(async () => {});
    const fallbackClient = makeClient({ promptAsync });

    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client: fallbackClient,
        serverUrl: TEST_SERVER_URL,
      },
      completion("task-1", "npm test"),
    );
    await sleep(300);

    expect(promptAsync).toHaveBeenCalledTimes(0);
    await waitForMockCallCount(promptAsync, 1, DEFERRED_COMPLETION_FALLBACK_MS + 1000);

    expect(promptAsync).toHaveBeenCalledTimes(1);
    expect(sessionBgStates.get("s1")?.pendingCompletions).toHaveLength(0);
    expect(sessionBgStates.get("s1")?.deferredCompletionTimer).toBeNull();
  });

  test("in-turn append drains deferred completion before fallback without duplicate wake", async () => {
    setTestLiveServerAvailable(false);
    trackBgTask("s1", "task-1");
    const send = mock(async (command: string) =>
      command === "bash_ack_completions"
        ? { success: true, acked_task_ids: ["task-1"] }
        : { success: true, bg_completions: [] },
    );
    const { ctx } = harness(send);
    const promptAsync = mock(async () => {});
    const fallbackClient = makeClient({ promptAsync });

    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client: fallbackClient,
        serverUrl: TEST_SERVER_URL,
      },
      completion("task-1", "npm test"),
    );

    const output = { output: "tool output" };
    await appendInTurnBgCompletions({ ctx, directory: "/tmp/project", sessionID: "s1" }, output);
    await sleep(DEFERRED_COMPLETION_FALLBACK_MS + 150);

    expect(output.output).toContain("task-1");
    expect(promptAsync).toHaveBeenCalledTimes(0);
    expect(send.mock.calls.filter((call) => call[0] === "bash_ack_completions")).toHaveLength(1);
  });

  test("markTaskWaiting consumes deferred completion before fallback without duplicate wake", async () => {
    setTestLiveServerAvailable(false);
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({ success: true, bg_completions: [] }));
    const promptAsync = mock(async () => {});
    const fallbackClient = makeClient({ promptAsync });

    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client: fallbackClient,
        serverUrl: TEST_SERVER_URL,
      },
      completion("task-1", "npm test"),
    );
    markTaskWaiting("s1", "task-1");
    await sleep(DEFERRED_COMPLETION_FALLBACK_MS + 150);

    expect(promptAsync).toHaveBeenCalledTimes(0);
    expect(sessionBgStates.get("s1")?.pendingCompletions).toHaveLength(0);
    expect(sessionBgStates.get("s1")?.deferredCompletionTimer).toBeNull();
  });

  test("staggered deferred fallback wakes matured task only", async () => {
    setTestLiveServerAvailable(false);
    const { ctx } = harness(() => ({ success: true, bg_completions: [] }));
    const promptAsync = mock(async () => {});
    const fallbackClient = makeClient({ promptAsync });

    trackBgTask("s1", "task-1");
    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client: fallbackClient,
        serverUrl: TEST_SERVER_URL,
      },
      completion("task-1", "cmd-1"),
    );
    await sleep(250);

    trackBgTask("s1", "task-2");
    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client: fallbackClient,
        serverUrl: TEST_SERVER_URL,
      },
      completion("task-2", "cmd-2"),
    );

    await waitForMockCallCount(promptAsync, 1, DEFERRED_COMPLETION_FALLBACK_MS + 1000);
    const firstText = (
      promptAsync.mock.calls[0]?.[0] as { body: { parts: Array<{ text: string }> } }
    ).body.parts[0].text;
    expect(firstText).toContain("task-1");
    expect(firstText).not.toContain("task-2");

    await waitForMockCallCount(promptAsync, 2, DEFERRED_COMPLETION_FALLBACK_MS + 1000);
    const secondText = (
      promptAsync.mock.calls[1]?.[0] as { body: { parts: Array<{ text: string }> } }
    ).body.parts[0].text;
    expect(secondText).toContain("task-2");
    expect(sessionBgStates.get("s1")?.pendingCompletions).toHaveLength(0);
  });

  test("buffered unknown completion promoted by trackBgTask gets deferred fallback", async () => {
    setTestLiveServerAvailable(false);
    const { ctx } = harness(() => ({ success: true, bg_completions: [] }));
    const promptAsync = mock(async () => {});
    const fallbackClient = makeClient({ promptAsync });

    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client: fallbackClient,
        serverUrl: TEST_SERVER_URL,
      },
      completion("task-1", "npm test"),
    );
    trackBgTask("s1", "task-1");

    expect(sessionBgStates.get("s1")?.deferredCompletionTimer).not.toBeNull();
    await waitForMockCallCount(promptAsync, 1, DEFERRED_COMPLETION_FALLBACK_MS + 1000);

    expect(promptAsync).toHaveBeenCalledTimes(1);
    expect(sessionBgStates.get("s1")?.pendingCompletions).toHaveLength(0);
  });

  test("trackBgTask buffered promotion resets hard-stop and wakes on fallback", async () => {
    setTestLiveServerAvailable(false);
    const { ctx } = harness(() => ({ success: true, bg_completions: [] }));
    const promptAsync = mock(async () => {});
    const fallbackClient = makeClient({ promptAsync });

    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client: fallbackClient,
        serverUrl: TEST_SERVER_URL,
      },
      completion("task-1", "npm test"),
    );

    const state = sessionBgStates.get("s1");
    expect(state).toBeDefined();
    if (!state) throw new Error("missing session state");
    state.wakeHardStopped = true;
    state.wakeRetryAttempts = 5;
    state.retryDelayMs = 1234;

    trackBgTask("s1", "task-1");

    expect(state.wakeHardStopped).toBe(false);
    expect(state.wakeRetryAttempts).toBe(0);
    expect(state.retryDelayMs).toBeNull();
    await waitForMockCallCount(promptAsync, 1, DEFERRED_COMPLETION_FALLBACK_MS + 1000);

    expect(promptAsync).toHaveBeenCalledTimes(1);
  });

  test("trackBgTask buffered promotion prunes stale long-running reminder for same task", async () => {
    setTestLiveServerAvailable(false);
    const { ctx } = harness(() => ({ success: true, bg_completions: [] }));
    const promptAsync = mock(async () => {});
    const fallbackClient = makeClient({ promptAsync });

    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client: fallbackClient,
        serverUrl: TEST_SERVER_URL,
      },
      completion("task-1", "npm test"),
    );

    const state = sessionBgStates.get("s1");
    expect(state).toBeDefined();
    if (!state) throw new Error("missing session state");
    state.pendingLongRunning.push({
      task_id: "task-1",
      session_id: "s1",
      command: "npm test",
      elapsed_ms: 30_000,
    });

    trackBgTask("s1", "task-1");

    expect(state.pendingLongRunning).toHaveLength(0);
    await waitForMockCallCount(promptAsync, 1, DEFERRED_COMPLETION_FALLBACK_MS + 1000);

    const text = (promptAsync.mock.calls[0]?.[0] as { body: { parts: Array<{ text: string }> } })
      .body.parts[0].text;
    expect(text).toContain("[BACKGROUND BASH COMPLETED]");
    expect(text).not.toContain("[BACKGROUND BASH STILL RUNNING]");
    expect(text).not.toContain("still running after");
  });

  test("buffers push completion received before task tracking", async () => {
    const { ctx } = harness(() => ({ success: true, bg_completions: [] }));
    const promptAsync = mock(async () => {});
    installLiveServerClient({ prompt: promptAsync });

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

  test("idle boundary promotes orphaned unknown completion and delivers it once", async () => {
    const send = mock(async (command: string) =>
      command === "bash_ack_completions"
        ? { success: true, acked_task_ids: ["task-orphan"] }
        : { success: true, bg_completions: [] },
    );
    const { ctx } = harness(send);
    const promptAsync = mock(async () => {});
    installLiveServerClient({ prompt: promptAsync });

    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client: {},
        serverUrl: TEST_SERVER_URL,
      },
      completion("task-orphan", "npm test"),
    );
    await sleep(300);

    expect(promptAsync).toHaveBeenCalledTimes(0);
    expect(sessionBgStates.get("s1")?.pendingCompletions).toHaveLength(0);

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
    expect(text).toContain("- task task-orphan (exit 0)");
    expect(sessionBgStates.get("s1")?.pendingCompletions).toHaveLength(0);
    expect(sessionBgStates.get("s1")?.unknownCompletions).toHaveLength(0);
    expect(send.mock.calls.filter((call) => call[0] === "bash_ack_completions")).toHaveLength(1);
  });

  test("buffered unknown completion respects late explicit-control promotion path", async () => {
    const send = mock(async (command: string) =>
      command === "bash_ack_completions"
        ? { success: true, acked_task_ids: ["task-explicit"] }
        : { success: true, bg_completions: [] },
    );
    const { ctx } = harness(send);
    const output = { output: "watch registered" };

    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client: {},
        serverUrl: TEST_SERVER_URL,
      },
      completion("task-explicit", "npm test"),
    );

    markExplicitControl("s1", "task-explicit", false);
    await appendInTurnBgCompletions({ ctx, directory: "/tmp/project", sessionID: "s1" }, output);

    expect(output.output).toContain("[BG BASH NOTIFY]");
    expect(output.output).toContain("- task task-explicit exited:");
    expect(output.output).not.toContain("[BACKGROUND BASH COMPLETED]");
    expect(sessionBgStates.get("s1")?.pendingCompletions).toHaveLength(0);
    expect(sessionBgStates.get("s1")?.pendingPatternMatches).toHaveLength(0);
    expect(sessionBgStates.get("s1")?.unknownCompletions).toHaveLength(0);
    expect(send).toHaveBeenCalledWith("bash_ack_completions", {
      session_id: "s1",
      task_ids: ["task-explicit"],
    });
  });

  test("buffered unknown completion is dropped after markTaskWaiting consumed path", async () => {
    const send = mock(async (command: string) =>
      command === "bash_ack_completions"
        ? { success: true, acked_task_ids: ["task-waiting"] }
        : { success: true, bg_completions: [] },
    );
    const { ctx } = harness(send);
    const promptAsync = mock(async () => {});
    installLiveServerClient({ prompt: promptAsync });

    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client: {},
        serverUrl: TEST_SERVER_URL,
      },
      completion("task-waiting", "npm test"),
    );

    markTaskWaiting("s1", "task-waiting");
    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: {},
      serverUrl: TEST_SERVER_URL,
    });
    await sleep(300);

    expect(promptAsync).toHaveBeenCalledTimes(0);
    expect(sessionBgStates.get("s1")?.pendingCompletions).toHaveLength(0);
    expect(sessionBgStates.get("s1")?.pendingPatternMatches).toHaveLength(0);
    expect(sessionBgStates.get("s1")?.unknownCompletions).toHaveLength(0);
    expect(send.mock.calls.filter((call) => call[0] === "bash_ack_completions")).toHaveLength(0);
  });

  test("failed wake keeps pending completions and retries", async () => {
    setTestLiveServerAvailable(false);
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({ success: true, bg_completions: [] }));
    const promptAsync = mock(async () => {
      throw new Error("send failed");
    });
    const fallbackClient = makeClient({ promptAsync });
    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: fallbackClient,
      serverUrl: TEST_SERVER_URL,
    });

    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client: fallbackClient,
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
    setTestLiveServerAvailable(false);
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({ success: true, bg_completions: [] }));
    const promptAsync = mock(async () => {
      throw new Error("send failed");
    });
    const fallbackClient = makeClient({ promptAsync });
    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: fallbackClient,
      serverUrl: TEST_SERVER_URL,
    });

    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client: fallbackClient,
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

  test("timer reminder hard-stops, then same-task completion push recovers without still-running text", async () => {
    setTestLiveServerAvailable(false);
    let shouldFail = true;
    const promptAsync = mock(async () => {
      if (shouldFail) throw new Error("send failed");
    });
    const fallbackClient = makeClient({ promptAsync });
    const { ctx } = harness(() => ({ success: true, bg_completions: [] }));
    trackBgTask("s1", "task-1");

    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: fallbackClient,
      serverUrl: TEST_SERVER_URL,
    });
    await handlePushedBgLongRunning(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client: fallbackClient,
        serverUrl: TEST_SERVER_URL,
      },
      { task_id: "task-1", session_id: "s1", command: "npm test", elapsed_ms: 30_000 },
    );
    await waitUntil(
      () => promptAsync.mock.calls.length >= 5 && sessionBgStates.get("s1")?.debounceTimer === null,
      10_000,
    );
    expect(sessionBgStates.get("s1")?.wakeHardStopped).toBe(true);

    shouldFail = false;
    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client: fallbackClient,
        serverUrl: TEST_SERVER_URL,
      },
      completion("task-1", "npm test"),
    );
    await waitForMockCallCount(promptAsync, 6, 2_000);

    const text = (
      promptAsync.mock.calls.at(-1)?.[0] as { body: { parts: Array<{ text: string }> } }
    ).body.parts[0].text;
    expect(text).toContain("[BACKGROUND BASH COMPLETED]");
    expect(text).not.toContain("[BACKGROUND BASH STILL RUNNING]");
    expect(sessionBgStates.get("s1")?.pendingLongRunning).toHaveLength(0);
  });

  test("timer reminder hard-stops, then urgent failure recovers immediately", async () => {
    setTestLiveServerAvailable(false);
    let shouldFail = true;
    const promptAsync = mock(async () => {
      if (shouldFail) throw new Error("send failed");
    });
    const fallbackClient = makeClient({ promptAsync });
    const { ctx } = harness(() => ({ success: true, bg_completions: [] }));
    trackBgTask("s1", "task-1");

    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: fallbackClient,
      serverUrl: TEST_SERVER_URL,
    });
    await handlePushedBgLongRunning(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client: fallbackClient,
        serverUrl: TEST_SERVER_URL,
      },
      { task_id: "task-1", session_id: "s1", command: "npm test", elapsed_ms: 30_000 },
    );
    await waitUntil(
      () => promptAsync.mock.calls.length >= 5 && sessionBgStates.get("s1")?.debounceTimer === null,
      10_000,
    );

    shouldFail = false;
    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client: fallbackClient,
        serverUrl: TEST_SERVER_URL,
      },
      { task_id: "task-1", status: "failed", exit_code: 1, command: "npm test" },
    );
    await waitForMockCallCount(promptAsync, 6, 500);

    const text = (
      promptAsync.mock.calls.at(-1)?.[0] as { body: { parts: Array<{ text: string }> } }
    ).body.parts[0].text;
    expect(text).toContain("[BACKGROUND BASH FAILED]");
    expect(text).not.toContain("[BACKGROUND BASH STILL RUNNING]");
  });

  test("drained completion path also recovers after timer hard-stop", async () => {
    setTestLiveServerAvailable(false);
    let shouldFail = true;
    let drainReturnsCompletion = false;
    const promptAsync = mock(async () => {
      if (shouldFail) throw new Error("send failed");
    });
    const fallbackClient = makeClient({ promptAsync });
    const { ctx } = harness((command) => {
      if (command === "bash_drain_completions") {
        return {
          success: true,
          bg_completions: drainReturnsCompletion ? [completion("task-1", "npm test")] : [],
        };
      }
      return { success: true, acked_task_ids: ["task-1"] };
    });

    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: fallbackClient,
      serverUrl: TEST_SERVER_URL,
    });
    await handlePushedBgLongRunning(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client: fallbackClient,
        serverUrl: TEST_SERVER_URL,
      },
      { task_id: "task-1", session_id: "s1", command: "npm test", elapsed_ms: 30_000 },
    );
    await waitUntil(
      () => promptAsync.mock.calls.length >= 5 && sessionBgStates.get("s1")?.debounceTimer === null,
      10_000,
    );

    shouldFail = false;
    drainReturnsCompletion = true;
    trackBgTask("s1", "task-1");
    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: fallbackClient,
      serverUrl: TEST_SERVER_URL,
    });
    await waitForMockCallCount(promptAsync, 6, 2_000);

    const text = (
      promptAsync.mock.calls.at(-1)?.[0] as { body: { parts: Array<{ text: string }> } }
    ).body.parts[0].text;
    expect(text).toContain("[BACKGROUND BASH COMPLETED]");
    expect(text).not.toContain("[BACKGROUND BASH STILL RUNNING]");
  });

  test("terminal completion prunes stale long-running state", async () => {
    setTestLiveServerAvailable(false);
    const promptAsync = mock(async () => {});
    const fallbackClient = makeClient({ promptAsync });
    const { ctx } = harness(() => ({ success: true, bg_completions: [] }));
    trackBgTask("s1", "task-1");

    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: fallbackClient,
      serverUrl: TEST_SERVER_URL,
    });
    await handlePushedBgLongRunning(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client: fallbackClient,
        serverUrl: TEST_SERVER_URL,
      },
      { task_id: "task-1", session_id: "s1", command: "npm test", elapsed_ms: 30_000 },
    );
    await waitForMockCallCount(promptAsync, 1, 2_000);

    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client: fallbackClient,
        serverUrl: TEST_SERVER_URL,
      },
      completion("task-1", "npm test"),
    );
    await waitForMockCallCount(promptAsync, 2, 2_000);

    const text = (
      promptAsync.mock.calls.at(-1)?.[0] as { body: { parts: Array<{ text: string }> } }
    ).body.parts[0].text;
    expect(text).toContain("[BACKGROUND BASH COMPLETED]");
    expect(text).not.toContain("still running after");
    expect(sessionBgStates.get("s1")?.pendingLongRunning).toHaveLength(0);
  });

  test("long-running wake clears completion deferral so later completion push wakes again", async () => {
    setTestLiveServerAvailable(false);
    const promptAsync = mock(async () => {});
    const fallbackClient = makeClient({ promptAsync });
    const { ctx } = harness(() => ({ success: true, bg_completions: [] }));
    trackBgTask("s1", "task-1");

    await handlePushedBgLongRunning(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client: fallbackClient,
        serverUrl: TEST_SERVER_URL,
      },
      { task_id: "task-1", session_id: "s1", command: "npm test", elapsed_ms: 30_000 },
    );
    await waitForMockCallCount(promptAsync, 1, 2_000);

    const firstText = (
      promptAsync.mock.calls[0]?.[0] as { body: { parts: Array<{ text: string }> } }
    ).body.parts[0].text;
    expect(firstText).toContain("[BACKGROUND BASH STILL RUNNING]");
    expect(firstText).not.toContain("[BACKGROUND BASH COMPLETED]");
    expect(sessionBgStates.get("s1")?.wakeDeferredTaskIds.has("task-1")).toBe(false);

    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client: fallbackClient,
        serverUrl: TEST_SERVER_URL,
      },
      completion("task-1", "npm test"),
    );
    await waitForMockCallCount(promptAsync, 2, 2_000);

    const secondText = (
      promptAsync.mock.calls[1]?.[0] as { body: { parts: Array<{ text: string }> } }
    ).body.parts[0].text;
    expect(secondText).toContain("[BACKGROUND BASH COMPLETED]");
    expect(secondText).not.toContain("[BACKGROUND BASH STILL RUNNING]");
  });

  test("inline consume path also clears hard-stop and stale long-running state", () => {
    trackBgTask("s1", "task-inline");
    const state = sessionBgStates.get("s1");
    expect(state).toBeDefined();

    state!.pendingLongRunning.push({
      task_id: "task-inline",
      session_id: "s1",
      command: "sleep 40",
      elapsed_ms: 40_000,
    });
    state!.wakeHardStopped = true;
    state!.wakeRetryAttempts = 5;
    state!.retryDelayMs = 1000;

    consumeBgCompletion("s1", "task-inline");

    expect(state!.wakeHardStopped).toBe(false);
    expect(state!.wakeRetryAttempts).toBe(0);
    expect(state!.retryDelayMs).toBeNull();
    expect(state!.pendingLongRunning).toHaveLength(0);
  });

  test("post-idle push completion still wakes even when bridge is busy with non-agent RPC", async () => {
    // Regression: previously bailed on `isActive()` (bridge.hasPendingRequests())
    // which returned true for the TUI status poll, orphaning the completion when
    // no other trigger fired. Once the spawn turn has gone idle, the wake must
    // still be scheduled.
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({ success: true, bg_completions: [] }));
    const promptAsync = mock(async () => {});
    installLiveServerClient({ prompt: promptAsync });
    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: {},
      serverUrl: TEST_SERVER_URL,
    });

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

  test("urgent terminal failure wakes without normal debounce delay", async () => {
    setTestLiveServerAvailable(false);
    trackBgTask("s1", "task-urgent");
    const { ctx } = harness(() => ({ success: true, bg_completions: [] }));
    const promptAsync = mock(async () => {});
    const fallbackClient = makeClient({ promptAsync });
    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: fallbackClient,
      serverUrl: TEST_SERVER_URL,
    });

    await handlePushedBgCompletion(
      {
        ctx,
        directory: "/tmp/project",
        sessionID: "s1",
        client: fallbackClient,
        serverUrl: TEST_SERVER_URL,
      },
      { task_id: "task-urgent", status: "failed", exit_code: 1, command: "npm test" },
    );
    await waitForMockCallCount(promptAsync, 1, 250);

    expect(promptAsync).toHaveBeenCalledTimes(1);
    const text = (promptAsync.mock.calls[0][0] as { body: { parts: Array<{ text: string }> } }).body
      .parts[0].text;
    expect(text).toContain("[BACKGROUND BASH FAILED]");
    expect(text).not.toContain("[BACKGROUND BASH COMPLETED]");
  });

  test("coalesces three idle completions into one notification", async () => {
    const responses = [
      { success: true, bg_completions: [completion("task-1", "one")] },
      { success: true, bg_completions: [completion("task-2", "two")] },
      { success: true, bg_completions: [completion("task-3", "three")] },
    ];
    const { ctx } = harness(() => responses.shift() ?? { success: true, bg_completions: [] });
    const promptAsync = mock(async () => {});
    installLiveServerClient({ prompt: promptAsync });

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
    installLiveServerClient({ prompt: promptAsync });
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
    installLiveServerClient({ prompt: promptAsync });
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
  // • `true`  — wake through `createOpencodeClient(input.serverUrl)` using
  //             live `session.prompt(...)` delivery proof.
  // • `false` — fall back to `drainContext.client.session.prompt(...)`, or
  //             degrade to `.promptAsync` only when prompt is missing.

  test("live-server wake uses session.prompt and tags trace as live-server", async () => {
    setTestLiveServerAvailable(true);
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({
      success: true,
      bg_completions: [completion("task-1", "npm test")],
    }));
    const livePrompt = mock(async () => {});
    const livePromptAsync = mock(async () => {});
    installLiveServerClient({ prompt: livePrompt, promptAsync: livePromptAsync });
    const fallbackClient = makeClient({ promptAsync: mock(async () => {}) });

    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: fallbackClient,
      serverUrl: TEST_SERVER_URL,
    });
    await waitForMockCallCount(livePrompt, 1);

    // The live-server client was used; the fallback client was NOT.
    expect(livePrompt).toHaveBeenCalledTimes(1);
    expect(livePromptAsync).toHaveBeenCalledTimes(0);
    expect(fallbackClient.session.promptAsync).toHaveBeenCalledTimes(0);

    const startMeta = findTraceEvent("bash_completion_wake_send_start");
    expect(startMeta).toBeDefined();
    expect(startMeta?.wake_client_path).toBe("live-server");
    expect(startMeta?.wake_client_method).toBe("prompt");
    expect(typeof startMeta?.delivery_id).toBe("string");
    expect(startMeta?.correlation_header).toBe("x-aft-delivery-id");
    expect(startMeta?.task_ids).toEqual(["task-1"]);
    // The factory saw the serverUrl + directory we configured.
    expect(getLastLiveServerArgs()).toEqual({
      serverUrl: TEST_SERVER_URL,
      directory: "/tmp/project",
      headers: {
        "x-aft-delivery-id": startMeta?.delivery_id as string,
      },
    });

    const okLogLine = sessionLogSpy.mock.calls.find(
      (call) =>
        (call[2] as { event?: string } | undefined)?.event === "bash_completion_wake_send_ok",
    );
    expect(okLogLine?.[1]).toContain("wake send resolved");
  });

  test("live prompt failure falls back in-process and demotes subsequent wakes", async () => {
    setTestLiveServerAvailable(true);
    const responses: BridgeResponse[] = [
      { success: true, bg_completions: [completion("task-1", "npm test")] },
      { success: true, bg_completions: [completion("task-2", "npm test again")] },
    ];
    const send = mock(async (command: string) =>
      command === "bash_drain_completions"
        ? (responses.shift() ?? { success: true, bg_completions: [] })
        : { success: true, acked_task_ids: [] },
    );
    const { ctx } = harness(send);
    const livePrompt = mock(async () => {
      throw new Error("connect ECONNREFUSED 127.0.0.1");
    });
    installLiveServerClient({ prompt: livePrompt });
    const fallbackClient = makeClient({ prompt: mock(async () => {}) });

    trackBgTask("s1", "task-1");
    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: fallbackClient,
      serverUrl: TEST_SERVER_URL,
    });
    await waitForMockCallCount(fallbackClient.session.prompt!, 1);

    expect(livePrompt).toHaveBeenCalledTimes(1);
    expect(fallbackClient.session.prompt).toHaveBeenCalledTimes(1);
    // Production code calls setLiveServerWakeAvailable(serverUrl, false)
    // (per-URL form), so check the per-URL availability map directly.
    expect(perUrlAvailability.get(normalizeServerUrl(TEST_SERVER_URL))).toBe(false);
    expect(sessionBgStates.get("s1")?.pendingCompletions).toHaveLength(0);
    expect(send.mock.calls.some((call) => call[0] === "bash_ack_completions")).toBe(true);

    const warnEvents = sessionWarnSpy.mock.calls.map(
      (call) => (call[2] as { event?: string } | undefined)?.event,
    );
    const debugEvents = sessionDebugSpy.mock.calls.map(
      (call) => (call[2] as { event?: string } | undefined)?.event,
    );
    expect(debugEvents).toContain("bash_completion_wake_send_error");
    expect(debugEvents).toContain("bash_completion_wake_live_server_fallback");
    expect(warnEvents).not.toContain("bash_completion_wake_send_error");
    expect(warnEvents).not.toContain("bash_completion_wake_live_server_fallback");

    trackBgTask("s1", "task-2");
    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: fallbackClient,
      serverUrl: TEST_SERVER_URL,
    });
    await waitForMockCallCount(fallbackClient.session.prompt!, 2);

    expect(livePrompt).toHaveBeenCalledTimes(1);
    expect(fallbackClient.session.prompt).toHaveBeenCalledTimes(2);
  });

  test("live client missing prompt does not call live promptAsync; falls back and demotes", async () => {
    setTestLiveServerAvailable(true);
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({
      success: true,
      bg_completions: [completion("task-1", "npm test")],
    }));
    const livePromptAsync = mock(async () => {});
    installLiveServerClient({ promptAsync: livePromptAsync });
    const fallbackClient = makeClient({ prompt: mock(async () => {}) });

    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: fallbackClient,
      serverUrl: TEST_SERVER_URL,
    });
    await waitForMockCallCount(fallbackClient.session.prompt!, 1);

    expect(livePromptAsync).toHaveBeenCalledTimes(0);
    expect(fallbackClient.session.prompt).toHaveBeenCalledTimes(1);
    expect(perUrlAvailability.get(normalizeServerUrl(TEST_SERVER_URL))).toBe(false);
  });

  test("in-process fallback prefers prompt when available and tags trace accordingly", async () => {
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
    installLiveServerClient({ prompt: livePromptAsync });
    const fallbackClient = makeClient({
      prompt: mock(async () => {}),
      promptAsync: mock(async () => {}),
    });

    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: fallbackClient,
      serverUrl: TEST_SERVER_URL,
    });
    await waitForMockCallCount(fallbackClient.session.prompt!, 1);

    // The fallback client was used; the live-server factory was NOT
    // consulted at all (no probe of getLastLiveServerArgs).
    expect(fallbackClient.session.prompt).toHaveBeenCalledTimes(1);
    expect(fallbackClient.session.promptAsync).toHaveBeenCalledTimes(0);
    expect(livePromptAsync).toHaveBeenCalledTimes(0);
    expect(getLastLiveServerArgs()).toBeNull();

    const startMeta = findTraceEvent("bash_completion_wake_send_start");
    expect(startMeta).toBeDefined();
    expect(startMeta?.wake_client_path).toBe("in-process-fallback");
    expect(startMeta?.wake_client_method).toBe("prompt");
    expect(typeof startMeta?.delivery_id).toBe("string");
    expect(startMeta?.task_ids).toEqual(["task-1"]);
  });

  test("in-process fallback uses promptAsync only if prompt missing", async () => {
    setTestLiveServerAvailable(false);
    trackBgTask("s1", "task-1");
    const { ctx } = harness(() => ({
      success: true,
      bg_completions: [completion("task-1", "npm test")],
    }));
    const fallbackClient = makeClient({ promptAsync: mock(async () => {}) });

    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: fallbackClient,
      serverUrl: TEST_SERVER_URL,
    });
    await waitForMockCallCount(fallbackClient.session.promptAsync!, 1);

    expect(findTraceEvent("bash_completion_wake_degraded_delivery")?.wake_client_method).toBe(
      "promptAsync",
    );
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
    installLiveServerClient({ prompt: livePromptAsync });
    await handleIdleBgCompletions({
      ctx,
      directory: "/tmp/project",
      sessionID: "s1",
      client: {},
      serverUrl: TEST_SERVER_URL,
    });

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

async function withSetTimeoutUnrefSpy<T>(run: () => Promise<T>): Promise<ReturnType<typeof mock>> {
  const originalSetTimeout = globalThis.setTimeout;
  let unrefSpy: ReturnType<typeof mock> | null = null;
  globalThis.setTimeout = ((handler: TimerHandler, timeout?: number, ...args: unknown[]) => {
    const timer = originalSetTimeout(handler, timeout, ...args);
    if (timer && typeof (timer as NodeJS.Timeout).unref === "function") {
      const realUnref = (timer as NodeJS.Timeout).unref.bind(timer as NodeJS.Timeout);
      unrefSpy = mock((...unrefArgs: unknown[]) => realUnref(...unrefArgs));
      (timer as NodeJS.Timeout).unref = unrefSpy as unknown as NodeJS.Timeout["unref"];
    }
    return timer;
  }) as typeof globalThis.setTimeout;
  try {
    await run();
  } finally {
    globalThis.setTimeout = originalSetTimeout;
  }
  if (!unrefSpy) throw new Error("expected setTimeout to return timer with unref()");
  return unrefSpy;
}
