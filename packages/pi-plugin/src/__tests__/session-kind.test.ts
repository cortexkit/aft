import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import type { BinaryBridge } from "@cortexkit/aft-bridge";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import {
  isPiWorkerSession,
  MAGIC_CONTEXT_SUBAGENT_ENV,
  piWorkerKind,
  skipsEagerStartup,
} from "../session-kind.js";
import { registerBashTool, watchCallerRole } from "../tools/bash.js";
import type { PluginContext } from "../types.js";

// Pi decides "is this a worker session" in one place. bash, bash_watch and
// extension startup all read it, and `bash.subagent_background: false` applies
// to exactly the sessions it names.

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

let savedSubagentEnv: string | undefined;

beforeEach(() => {
  savedSubagentEnv = process.env[MAGIC_CONTEXT_SUBAGENT_ENV];
  delete process.env[MAGIC_CONTEXT_SUBAGENT_ENV];
});

afterEach(() => {
  if (savedSubagentEnv === undefined) delete process.env[MAGIC_CONTEXT_SUBAGENT_ENV];
  else process.env[MAGIC_CONTEXT_SUBAGENT_ENV] = savedSubagentEnv;
});

function bashTools(
  send: (command: string, params: Record<string, unknown>) => Record<string, unknown>,
  bashConfig: Record<string, unknown>,
) {
  const calls: Array<[string, Record<string, unknown>]> = [];
  const bridge = {
    send: async (command: string, params: Record<string, unknown>) => {
      calls.push([command, params]);
      return send(command, params);
    },
  } as unknown as BinaryBridge;
  const ctx = {
    pool: { getBridge: () => bridge } as PluginContext["pool"],
    config: { bash: bashConfig } as PluginContext["config"],
    storageDir: "/tmp/test",
  } satisfies PluginContext;
  const tools = new Map<string, MockToolDef>();
  registerBashTool(
    { registerTool: (tool: MockToolDef) => tools.set(tool.name, tool) } as unknown as ExtensionAPI,
    ctx,
  );
  const get = (name: string) => {
    const tool = tools.get(name);
    if (!tool) throw new Error(`${name} was not registered`);
    return tool;
  };
  return { calls, bash: get("bash"), watch: get("bash_watch") };
}

const completed = () => ({ success: true, status: "completed", output: "done", exit_code: 0 });

async function run(
  tool: MockToolDef,
  params: Record<string, unknown>,
  hasUI: boolean,
): Promise<ToolResult> {
  return (await tool.execute("call", params, undefined, undefined, {
    cwd: process.cwd(),
    hasUI,
  })) as ToolResult;
}

describe("Pi worker-session predicate", () => {
  test("names why a session is a worker", () => {
    expect(piWorkerKind({ hasUI: false }, {})).toBe("headless");
    expect(piWorkerKind({ hasUI: true }, { [MAGIC_CONTEXT_SUBAGENT_ENV]: "1" })).toBe("delegated");
    expect(piWorkerKind({ hasUI: true }, {})).toBeUndefined();
    expect(piWorkerKind(undefined, {})).toBeUndefined();
    expect(isPiWorkerSession({ hasUI: false }, {})).toBe(true);
  });

  test("bash_watch's role and the shared predicate agree", () => {
    for (const hasUI of [true, false, undefined]) {
      for (const env of [{}, { [MAGIC_CONTEXT_SUBAGENT_ENV]: "1" }]) {
        const ctx = hasUI === undefined ? undefined : { hasUI };
        expect(watchCallerRole(ctx, env) === "worker").toBe(isPiWorkerSession(ctx, env));
      }
    }
  });

  test("only a delegated child skips eager startup; plain headless keeps it", () => {
    expect(skipsEagerStartup({ [MAGIC_CONTEXT_SUBAGENT_ENV]: "1" })).toBe(true);
    expect(skipsEagerStartup({})).toBe(false);
  });
});

describe("Pi bash.subagent_background", () => {
  const disabled = { background: true, subagent_background: false };

  test("false makes a headless worker's background request block to completion", async () => {
    const { calls, bash } = bashTools(completed, disabled);
    await run(bash, { command: "sleep 1", background: true }, false);
    const [command, params] = calls[0] ?? [];
    expect(command).toBe("bash");
    expect(params?.background).toBe(false);
    expect(params?.notify_on_completion).toBe(false);
    expect(params?.block_to_completion).toBe(true);
  });

  test("false also applies to a delegated child that has a UI context", async () => {
    process.env[MAGIC_CONTEXT_SUBAGENT_ENV] = "1";
    const { calls, bash } = bashTools(completed, disabled);
    await run(bash, { command: "sleep 1", background: true }, true);
    expect(calls[0]?.[1].block_to_completion).toBe(true);
    expect(calls[0]?.[1].background).toBe(false);
  });

  test("false leaves an interactive primary's background request alone", async () => {
    const { calls, bash } = bashTools(completed, disabled);
    await run(bash, { command: "sleep 1", background: true }, true);
    expect(calls[0]?.[1].background).toBe(true);
    expect(calls[0]?.[1].block_to_completion).toBe(false);
  });

  test("the default (true) lets a headless worker background", async () => {
    const { calls, bash } = bashTools(completed, { background: true });
    await run(bash, { command: "sleep 1", background: true }, false);
    expect(calls[0]?.[1].background).toBe(true);
    expect(calls[0]?.[1].block_to_completion).toBe(false);
  });

  test("false refuses pty for a worker instead of running it in the foreground", async () => {
    const { calls, bash } = bashTools(completed, disabled);
    await expect(run(bash, { command: "python", pty: true }, false)).rejects.toThrow(
      "bash.subagent_background is false",
    );
    expect(calls).toHaveLength(0);
  });

  test("false turns a worker's async bash_watch into a sync wait for the cap", async () => {
    const { calls, watch } = bashTools(
      (command) =>
        command === "bash_status"
          ? { success: true, status: "completed", exit_code: 0, output: "" }
          : { success: true, watch_id: "w1" },
      { ...disabled, watch_sync_max_ms: 90_000 },
    );
    const result = await run(
      watch,
      { task_id: "bash-forced", pattern: "ready", background: true },
      false,
    );
    expect(calls.map(([command]) => command)).not.toContain("bash_notify");
    expect(result.content[0]?.text).not.toContain("Watch registered");
    expect(result.details.effectiveWaitMs).toBe(90_000);
  });
});
