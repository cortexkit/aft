import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import type { BinaryBridge } from "@cortexkit/aft-bridge";
import { watchClock, watchTimeoutSteer } from "@cortexkit/aft-bridge";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { __resetSyncWatchAbortForTests, signalSyncWatchAbort } from "../sync-watch-abort.js";
import { resolveSessionId } from "../tools/_shared.js";
import { registerBashTool, watchCallerRole } from "../tools/bash.js";
import type { PluginContext } from "../types.js";

// bash_watch words its timeout reply, and picks its default deadline, by the
// caller's role. Pi has no parent-session link, so a headless context
// (`hasUI: false`, the `pi --print` children delegated agents run in) is the
// worker signal; interactive and RPC contexts have a UI and are primary.

interface MockToolDef {
  name: string;
  execute: (
    toolCallId: string,
    params: Record<string, unknown>,
    signal: AbortSignal | undefined,
    onUpdate: ((update: unknown) => void) | undefined,
    ctx: { cwd: string; hasUI?: boolean },
  ) => Promise<unknown>;
}

type ToolResult = {
  content: Array<{ type: string; text: string }>;
  details: Record<string, unknown>;
};

const SUBAGENT_ENV = "MAGIC_CONTEXT_PI_SUBAGENT";
let savedSubagentEnv: string | undefined;

beforeEach(() => {
  savedSubagentEnv = process.env[SUBAGENT_ENV];
  delete process.env[SUBAGENT_ENV];
});

afterEach(() => {
  if (savedSubagentEnv === undefined) delete process.env[SUBAGENT_ENV];
  else process.env[SUBAGENT_ENV] = savedSubagentEnv;
});

function registeredTool(
  name: string,
  send: (
    command: string,
    params: Record<string, unknown>,
    options?: Record<string, unknown>,
  ) => Record<string, unknown>,
  config: Record<string, unknown> = {},
): MockToolDef {
  const bridge = {
    send: async (
      command: string,
      params: Record<string, unknown>,
      options?: Record<string, unknown>,
    ) => send(command, params, options),
  } as unknown as BinaryBridge;
  const ctx = {
    pool: { getBridge: () => bridge } as PluginContext["pool"],
    config: config as PluginContext["config"],
    storageDir: "/tmp/test",
  } satisfies PluginContext;
  const tools = new Map<string, MockToolDef>();
  registerBashTool(
    { registerTool: (tool: MockToolDef) => tools.set(tool.name, tool) } as unknown as ExtensionAPI,
    ctx,
  );
  const tool = tools.get(name);
  if (!tool) throw new Error(`${name} was not registered`);
  return tool;
}

function watchTool(
  send: (command: string) => Record<string, unknown>,
  config: Record<string, unknown> = {},
): MockToolDef {
  return registeredTool("bash_watch", (command) => send(command), config);
}

async function watch(
  tool: MockToolDef,
  params: Record<string, unknown>,
  hasUI: boolean,
  signal?: AbortSignal,
): Promise<ToolResult> {
  return (await tool.execute("call", params, signal, undefined, {
    cwd: process.cwd(),
    hasUI,
  })) as ToolResult;
}

/**
 * Replaces the bash_watch clock with simulated time: each sleep advances the
 * clock by its duration and returns at once, so minutes of waiting run in
 * milliseconds. Call `restore` when done.
 */
function useFakeWatchClock(): { now: () => number; restore: () => void } {
  const real = { now: watchClock.now, sleep: watchClock.sleep };
  let nowMs = 0;
  watchClock.now = () => nowMs;
  watchClock.sleep = async (ms: number) => {
    nowMs += ms;
  };
  return {
    now: () => nowMs,
    restore: () => {
      watchClock.now = real.now;
      watchClock.sleep = real.sleep;
    },
  };
}

describe("Pi bash_watch caller role", () => {
  test("watchCallerRole: headless or pi-magic-context subagent is a worker, UI is primary", () => {
    expect(watchCallerRole({ hasUI: false }, {})).toBe("worker");
    expect(watchCallerRole({ hasUI: true }, {})).toBe("primary");
    expect(watchCallerRole(undefined, {})).toBe("primary");
    expect(watchCallerRole({ hasUI: true }, { [SUBAGENT_ENV]: "1" })).toBe("worker");
  });

  test("timeout gives a headless worker only the worker steer, with no cap", async () => {
    const tool = watchTool(
      () => ({ success: true, status: "running", duration_ms: 4_000, output_preview: "tick\n" }),
      { bash: { watch_sync_max_ms: 90_000 } },
    );
    const result = await watch(tool, { task_id: "bash-worker", timeout_ms: 1 }, false);
    const text = result.content[0].text;
    expect(text).toContain("timeout reached without match");
    expect(text).toContain("The command is still running after");
    expect(text).toContain('Call bash_watch({ task_id: "bash-worker" }) again to keep waiting');
    expect(text).toContain("without timeout_ms a watch waits up to the worker wait limit");
    expect(text).toContain('bash_kill({ task_id: "bash-worker" })');
    expect(text).toContain("It has run for 4s.");
    expect(text).toContain("Recent output:\ntick");
    expect(text).not.toContain("waits until the command finishes");
    expect(text).not.toContain("90000");
    expect(text).not.toContain("end your turn");
    expect(text).not.toContain("don't poll");
    expect(text).not.toContain("completion reminder");
  });

  test("timeout gives an interactive primary only the primary steer", async () => {
    const tool = watchTool(() => ({ success: true, status: "running" }));
    const result = await watch(tool, { task_id: "bash-primary", timeout_ms: 1 }, true);
    const text = result.content[0].text;
    expect(text).toContain("timeout reached without match");
    expect(text).toContain(watchTimeoutSteer());
    expect(text).not.toContain("don't report a result");
  });

  test("without timeout_ms a worker's watch waits up to the worker wait limit and a primary's 30000", async () => {
    const effectiveFor = async (hasUI: boolean, config: Record<string, unknown> = {}) => {
      let polls = 0;
      const tool = watchTool(() => {
        polls += 1;
        return polls === 1
          ? { success: true, status: "running" }
          : { success: true, status: "completed", exit_code: 0 };
      }, config);
      const result = await watch(tool, { task_id: `bash-default-${hasUI}` }, hasUI);
      return result.details.effectiveWaitMs;
    };
    expect(await effectiveFor(false)).toBe(1_800_000);
    expect(await effectiveFor(false, { bash: { worker_wait_max_ms: 300_000 } })).toBe(300_000);
    expect(await effectiveFor(true)).toBe(30_000);
  });

  // The rest run on simulated time (useFakeWatchClock), so a wait of several
  // minutes finishes in milliseconds.

  test("a worker's watch without timeout_ms returns when the task exits, past 120 s and before the limit", async () => {
    __resetSyncWatchAbortForTests();
    const clock = useFakeWatchClock();
    try {
      let polls = 0;
      const tool = watchTool(() => {
        polls += 1;
        return clock.now() < 150_000
          ? { success: true, status: "running" }
          : { success: true, status: "completed", exit_code: 0 };
      });
      const text = (await watch(tool, { task_id: "bash-worker-long" }, false)).content[0].text;
      expect(text).toContain("task exited (completed, exit 0)");
      expect(text).toContain("(limit 1800000ms)");
      expect(text).not.toContain("timeout reached");
      expect(Number(/Waited (\d+)ms/.exec(text)?.[1])).toBeGreaterThanOrEqual(150_000);
      // The poll interval backs off, so a long wait stays cheap: at a fixed
      // 100 ms this wait would have taken 1500 status polls.
      expect(polls).toBeLessThan(500);
    } finally {
      clock.restore();
    }
  });

  // The worker wait limit: a stuck command once held a worker for fifteen
  // hours behind a watch with no deadline.
  test("a worker's watch without timeout_ms returns still-running at the worker wait limit", async () => {
    __resetSyncWatchAbortForTests();
    const clock = useFakeWatchClock();
    try {
      const tool = watchTool(() => ({
        success: true,
        status: "running",
        mode: "pipes",
        output_preview: "(pass) one\n",
      }));
      const text = (await watch(tool, { task_id: "bash-worker-stuck" }, false)).content[0].text;
      expect(text).toMatch(/Waited 1800000ms \(limit 1800000ms\); timeout reached without match/);
      expect(text).toContain("The command is still running after 30 minutes of watching");
      expect(text).toContain('Call bash_watch({ task_id: "bash-worker-stuck" }) again');
      expect(text).toContain("Recent output:\n(pass) one");
      expect(clock.now()).toBeLessThan(1_802_000);
    } finally {
      clock.restore();
    }
  });

  test("a worker bash_watch at its cap waits for a timeout just inside the handoff margin", async () => {
    __resetSyncWatchAbortForTests();
    const clock = useFakeWatchClock();
    const killAtMs = 1_802_000;
    const reason = "killed by the explicit timeout (exit 124)";
    try {
      const tool = watchTool(() =>
        clock.now() < killAtMs
          ? {
              success: true,
              status: "running",
              started_at: Date.now(),
              hard_kill: { limit_ms: killAtMs, source: "timeout" },
              elapsed_ms: Math.round(clock.now()),
            }
          : { success: true, status: "timed_out", exit_code: 124, status_reason: reason },
      );
      const text = (await watch(tool, { task_id: "bash-worker-timeout-at-cap" }, false)).content[0]
        .text;
      expect(text).toContain("task exited (timed_out, exit 124)");
      expect(text).toContain("The task was killed by the explicit timeout (exit 124).");
      expect(text).not.toContain("timeout reached without match");
      expect(text).not.toContain("The command is still running");
      expect(clock.now()).toBeGreaterThanOrEqual(killAtMs);
    } finally {
      clock.restore();
    }
  });

  // A worker once watched a background task with "no limit", never learned
  // the task had AFT's 30-minute default kill, and took the kill for its
  // command failing.
  test("a worker's watch names the task's kill deadline and the default limit that killed it", async () => {
    __resetSyncWatchAbortForTests();
    const clock = useFakeWatchClock();
    const reason =
      "killed by AFT's default background limit of 30 minutes (exit 124); the command itself did not fail, pass a longer `timeout` if it needs more time";
    try {
      const tool = watchTool(() =>
        clock.now() < 1_000_000
          ? {
              success: true,
              status: "running",
              started_at: Date.now(),
              hard_kill: { limit_ms: 1_800_000, source: "default" },
            }
          : { success: true, status: "timed_out", exit_code: 124, status_reason: reason },
      );
      const running = (
        await watch(tool, { task_id: "bash-worker-deadline", timeout_ms: 60_000 }, false)
      ).content[0].text;
      expect(running).toContain("when it has run 30 minutes (its default background limit)");
      expect(running).toContain("remain.");
      const killed = (await watch(tool, { task_id: "bash-worker-deadline" }, false)).content[0]
        .text;
      expect(killed).toContain("task exited (timed_out");
      expect(killed).toContain(
        "The task was killed by AFT's default background limit of 30 minutes (exit 124)",
      );
      expect(killed).not.toContain("no limit");
    } finally {
      clock.restore();
    }
  });

  test("a worker's second watch keeps waiting and returns the result", async () => {
    __resetSyncWatchAbortForTests();
    const clock = useFakeWatchClock();
    try {
      const tool = watchTool(() =>
        clock.now() < 2_400_000
          ? { success: true, status: "running" }
          : { success: true, status: "completed", exit_code: 0, output_preview: "all green\n" },
      );
      const first = (await watch(tool, { task_id: "bash-worker-twice" }, false)).content[0].text;
      expect(first).toContain("still running after 30 minutes of watching");
      const second = (await watch(tool, { task_id: "bash-worker-twice" }, false)).content[0].text;
      expect(second).toContain("task exited (completed, exit 0)");
      expect(second).toContain("all green");
    } finally {
      clock.restore();
    }
  });

  test("a worker's watch honours a configured worker_wait_max_ms", async () => {
    __resetSyncWatchAbortForTests();
    const clock = useFakeWatchClock();
    try {
      const tool = watchTool(() => ({ success: true, status: "running" }), {
        bash: { worker_wait_max_ms: 300_000 },
      });
      const text = (await watch(tool, { task_id: "bash-worker-configured" }, false)).content[0]
        .text;
      expect(text).toMatch(/Waited 300000ms \(limit 300000ms\); timeout reached without match/);
      expect(text).toContain("still running after 5 minutes of watching");
    } finally {
      clock.restore();
    }
  });

  test("a new message ends a worker's watch before its limit", async () => {
    __resetSyncWatchAbortForTests();
    const clock = useFakeWatchClock();
    const sessionId = resolveSessionId({ cwd: process.cwd(), hasUI: false } as never);
    try {
      const tool = watchTool(() => {
        if (clock.now() >= 200_000) signalSyncWatchAbort(sessionId);
        return { success: true, status: "running", mode: "pipes" };
      });
      const text = (await watch(tool, { task_id: "bash-worker-message" }, false)).content[0].text;
      expect(text).toContain("interrupted because you sent a message");
      expect(text).toContain("Call bash_watch again to keep waiting");
      expect(text).not.toContain("completion reminder");
      expect(clock.now()).toBeGreaterThanOrEqual(200_000);
      expect(clock.now()).toBeLessThan(202_000);
    } finally {
      clock.restore();
    }
  });

  test("an aborted tool call ends a worker's watch before its limit", async () => {
    __resetSyncWatchAbortForTests();
    const clock = useFakeWatchClock();
    const controller = new AbortController();
    try {
      const tool = watchTool(() => {
        if (clock.now() >= 300_000) controller.abort();
        return { success: true, status: "running" };
      });
      const text = (await watch(tool, { task_id: "bash-worker-abort" }, false, controller.signal))
        .content[0].text;
      expect(text).toContain("the watch was cancelled");
      expect(clock.now()).toBeLessThan(302_000);
    } finally {
      clock.restore();
    }
  });

  test("a worker's explicit timeout_ms above the cap is honoured as given", async () => {
    __resetSyncWatchAbortForTests();
    const clock = useFakeWatchClock();
    try {
      const tool = watchTool(() => ({ success: true, status: "running" }));
      const text = (
        await watch(tool, { task_id: "bash-worker-explicit", timeout_ms: 600_000 }, false)
      ).content[0].text;
      expect(text).toMatch(/Waited 600000ms \(limit 600000ms\); timeout reached without match/);
    } finally {
      clock.restore();
    }
  });

  test("a primary's watch without timeout_ms still ends at 30 s", async () => {
    __resetSyncWatchAbortForTests();
    const clock = useFakeWatchClock();
    try {
      const tool = watchTool(() => ({ success: true, status: "running" }));
      const text = (await watch(tool, { task_id: "bash-primary-default" }, true)).content[0].text;
      expect(text).toMatch(/Waited 30000ms \(limit 30000ms\); timeout reached without match/);
      expect(text).toContain(watchTimeoutSteer());
    } finally {
      clock.restore();
    }
  });
});

describe("Pi bash wait:true caller role", () => {
  async function waitCall(
    params: Record<string, unknown>,
    hasUI: boolean,
    config: Record<string, unknown> = {},
  ) {
    const calls: Array<[Record<string, unknown>, Record<string, unknown> | undefined]> = [];
    const tool = registeredTool(
      "bash",
      (_command, sent, options) => {
        calls.push([sent, options]);
        return {
          success: true,
          status: "completed",
          task_id: "task-wait",
          exit_code: 0,
          output: "ok",
        };
      },
      config,
    );
    await tool.execute("call", { command: "long-build", ...params }, undefined, undefined, {
      cwd: process.cwd(),
      hasUI,
    });
    return calls[0];
  }

  test("a worker's wait:true without a timeout gets a transport budget of the worker wait limit", async () => {
    const [params, options] = await waitCall({ wait: true }, false);
    expect(params.wait).toBe(true);
    expect(params.timeout).toBeUndefined();
    expect(params.worker_session).toBe(true);
    // The engine hands the call back at the worker wait limit (30 minutes by
    // default), so the transport waits that long plus its margin.
    expect(options?.transportTimeoutMs).toBe(1_800_000 + 10_000);
  });

  test("a worker's wait:true with an explicit timeout keeps it", async () => {
    const [params, options] = await waitCall({ wait: true, timeout: 45_000 }, false);
    expect(params.timeout).toBe(45_000);
    expect(options?.transportTimeoutMs).toBe(45_000 + 10_000);
  });

  test("a primary's wait:true without a timeout keeps the 30-minute budget", async () => {
    const [params, options] = await waitCall({ wait: true }, true);
    expect(params).not.toHaveProperty("worker_session");
    expect(options?.transportTimeoutMs).toBe(30 * 60 * 1000 + 10_000);
  });

  test("a primary's blocking transport budget follows worker_wait_max_ms without extending command timeout", async () => {
    for (const [timeout, expected] of [
      [undefined, 7_200_000],
      [9_000_000, 7_200_000],
      [45_000, 45_000],
    ] as const) {
      const [params, options] = await waitCall({ wait: true, timeout }, true, {
        bash: { worker_wait_max_ms: 7_200_000 },
      });
      expect(params.timeout).toBe(timeout);
      expect(options?.transportTimeoutMs).toBe(expected + 10_000);
    }
  });

  test("a worker's requests carry its role and the engine's hand-off text gets the bash_watch note", async () => {
    const sent: Array<Record<string, unknown>> = [];
    // The engine's reply to a request with worker_session says the task won't wake it.
    const handOff =
      "Background task started: bash-bg. It won't wake you when it finishes, so wait for it before you report a result.";
    const tool = registeredTool("bash", (_command, params) => {
      sent.push(params);
      return { success: true, status: "running", task_id: "bash-bg", output: handOff };
    });
    const result = (await tool.execute(
      "call",
      { command: "sleep 30", background: true },
      undefined,
      undefined,
      { cwd: process.cwd(), hasUI: false },
    )) as ToolResult;
    expect(sent[0]?.worker_session).toBe(true);
    // The engine cannot assume the host has bash_watch, so the plugin adds
    // how to wait on the task, in Pi's argument spelling.
    expect(result.content[0].text.startsWith(handOff)).toBe(true);
    expect(result.content[0].text).toContain('bash_watch({ task_id: "bash-bg" })');
    expect(result.content[0].text).toContain("waits up to the worker wait limit");
  });

  test("a worker's bash_watch status polls carry its role too", async () => {
    const sent: Array<Record<string, unknown>> = [];
    const tool = registeredTool("bash_watch", (_command, params) => {
      sent.push(params);
      return { success: true, status: "completed", exit_code: 0 };
    });
    await watch(tool, { task_id: "bash-role" }, false);
    expect(sent.every((params) => params.worker_session === true)).toBe(true);
    await watch(tool, { task_id: "bash-role" }, true);
    expect(sent.at(-1)).not.toHaveProperty("worker_session");
  });
});

describe("Pi tool_call caller role", () => {
  test("a worker's tool_call carries its role in the envelope; a primary's does not", async () => {
    const { callToolCall } = await import("../tools/_shared.js");
    const options: Array<Record<string, unknown> | undefined> = [];
    const bridge = {
      toolCall: async (
        _sessionId: string | undefined,
        _name: string,
        _args: Record<string, unknown>,
        sent?: Record<string, unknown>,
      ) => {
        options.push(sent);
        return { success: true, text: "ok" };
      },
    } as unknown as Parameters<typeof callToolCall>[0];
    await callToolCall(bridge, "read", { filePath: "a.ts" }, {
      cwd: process.cwd(),
      hasUI: false,
    } as never);
    await callToolCall(bridge, "read", { filePath: "a.ts" }, {
      cwd: process.cwd(),
      hasUI: true,
    } as never);
    expect(options[0]?.workerSession).toBe(true);
    expect(options[1]).not.toHaveProperty("workerSession");
  });
});
