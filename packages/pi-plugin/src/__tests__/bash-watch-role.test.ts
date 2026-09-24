import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import type { BinaryBridge } from "@cortexkit/aft-bridge";
import { watchTimeoutSteer } from "@cortexkit/aft-bridge";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
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

function watchTool(
  send: (command: string) => Record<string, unknown>,
  config: Record<string, unknown> = {},
): MockToolDef {
  const bridge = {
    send: async (command: string) => send(command),
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
  const tool = tools.get("bash_watch");
  if (!tool) throw new Error("bash_watch was not registered");
  return tool;
}

async function watch(
  tool: MockToolDef,
  params: Record<string, unknown>,
  hasUI: boolean,
): Promise<ToolResult> {
  return (await tool.execute("call", params, undefined, undefined, {
    cwd: process.cwd(),
    hasUI,
  })) as ToolResult;
}

describe("Pi bash_watch caller role", () => {
  test("watchCallerRole: headless or pi-magic-context subagent is a worker, UI is primary", () => {
    expect(watchCallerRole({ hasUI: false }, {})).toBe("worker");
    expect(watchCallerRole({ hasUI: true }, {})).toBe("primary");
    expect(watchCallerRole(undefined, {})).toBe("primary");
    expect(watchCallerRole({ hasUI: true }, { [SUBAGENT_ENV]: "1" })).toBe("worker");
  });

  test("timeout gives a headless worker only the worker steer with the resolved cap", async () => {
    const tool = watchTool(() => ({ success: true, status: "running" }), {
      bash: { watch_sync_max_ms: 90_000 },
    });
    const result = await watch(tool, { task_id: "bash-worker", timeout_ms: 1 }, false);
    const text = result.content[0].text;
    expect(text).toContain("timeout reached without match");
    expect(text).toContain(watchTimeoutSteer("worker", 90_000, "timeout_ms"));
    expect(text).toContain("timeout_ms up to 90000");
    expect(text).not.toContain("end your turn");
    expect(text).not.toContain("don't poll");
  });

  test("timeout gives an interactive primary only the primary steer", async () => {
    const tool = watchTool(() => ({ success: true, status: "running" }));
    const result = await watch(tool, { task_id: "bash-primary", timeout_ms: 1 }, true);
    const text = result.content[0].text;
    expect(text).toContain("timeout reached without match");
    expect(text).toContain(watchTimeoutSteer("primary", 120_000));
    expect(text).not.toContain("don't report a result");
  });

  test("without timeout_ms a worker waits up to the cap and a primary 30000", async () => {
    const effectiveFor = async (hasUI: boolean) => {
      let polls = 0;
      const tool = watchTool(() => {
        polls += 1;
        return polls === 1
          ? { success: true, status: "running" }
          : { success: true, status: "completed", exit_code: 0 };
      });
      const result = await watch(tool, { task_id: `bash-default-${hasUI}` }, hasUI);
      return result.details.effectiveWaitMs;
    };
    expect(await effectiveFor(false)).toBe(120_000);
    expect(await effectiveFor(true)).toBe(30_000);
  });
});
