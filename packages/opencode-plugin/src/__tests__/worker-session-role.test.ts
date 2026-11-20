/**
 * The OpenCode plugin tells AFT the caller's role on every request: a
 * delegated worker (subagent) session gets `worker_session: true` beside its
 * `session_id`, a primary session gets nothing. AFT words its replies by it
 * (no completion-reminder promise, no "end the turn" for a worker), so both
 * shared bridge helpers must carry it.
 */
import { describe, expect, test } from "bun:test";
import { resolve } from "node:path";
import { type BridgePool, BridgeTransportUnavailableError } from "@cortexkit/aft-bridge";
import { _resetSubagentCacheForTest, LOOKUP_TIMEOUT_MS } from "../shared/subagent-detect.js";
import { callBridge, callToolCall } from "../tools/_shared.js";
import type { PluginContext } from "../types.js";

const PROJECT_CWD = resolve(import.meta.dir, "../../../..");

function harness(parentID: string | undefined, hang = false, toolCallError?: Error) {
  let lookups = 0;
  const sends: Array<Record<string, unknown>> = [];
  const toolCalls: Array<Record<string, unknown> | undefined> = [];
  const bridge = {
    send: async (_command: string, params: Record<string, unknown>) => {
      sends.push(params);
      return { success: true };
    },
    toolCall: async (
      _sessionId: string | undefined,
      _name: string,
      _args: Record<string, unknown>,
      options?: Record<string, unknown>,
    ) => {
      toolCalls.push(options);
      if (toolCallError) throw toolCallError;
      return { success: true, text: "ok" };
    },
  };
  const ctx = {
    pool: { getBridge: () => bridge } as unknown as BridgePool,
    client: {
      session: {
        // callBridge starts the role lookup first, alongside the directory
        // lookup. Only the role lookup hangs; directory resolution still works.
        get: async (input: { path: { id: string } }) =>
          hang && ++lookups === 1
            ? new Promise<never>(() => {})
            : { data: { id: input.path.id, parentID, directory: PROJECT_CWD } },
      },
    },
    config: {},
    storageDir: "/tmp/aft-test",
  } as unknown as PluginContext;
  return { ctx, sends, toolCalls };
}

const runtime = (sessionID: string) => ({
  sessionID,
  directory: PROJECT_CWD,
  worktree: PROJECT_CWD,
});

describe("caller role on every bridge request", () => {
  test("a subagent's requests carry worker_session except for TUI status reads", async () => {
    _resetSubagentCacheForTest();
    const { ctx, sends, toolCalls } = harness("ses_parent");
    const worker = runtime("ses_role_worker");
    await callBridge(ctx, worker, "bash_status", { task_id: "t" });
    await callBridge(ctx, { ...worker, observeBashStatus: true }, "bash_status", { task_id: "t" });
    await callToolCall(ctx, worker, "read", { filePath: "a.ts" });
    expect(sends[0]).toMatchObject({ session_id: "ses_role_worker", worker_session: true });
    expect(sends[1]).toMatchObject({ session_id: "ses_role_worker", task_id: "t" });
    expect(sends[1]).not.toHaveProperty("worker_session");
    expect(toolCalls[0]?.workerSession).toBe(true);
  });

  test("a primary session's requests carry no role", async () => {
    _resetSubagentCacheForTest();
    const { ctx, sends, toolCalls } = harness(undefined);
    await callBridge(ctx, runtime("ses_role_primary"), "bash_status", { task_id: "t" });
    await callToolCall(ctx, runtime("ses_role_primary"), "read", { filePath: "a.ts" });
    expect(sends[0]).not.toHaveProperty("worker_session");
    expect(toolCalls[0]).not.toHaveProperty("workerSession");
  });

  // A slow host must not delay ordinary tool calls (each one asks for the
  // role): the lookup is abandoned after LOOKUP_TIMEOUT_MS and the call goes
  // ahead as a primary session.
  test("a host that never answers delays a bridge call by at most the lookup timeout", async () => {
    _resetSubagentCacheForTest();
    const { ctx, sends } = harness("ses_parent", true);
    const started = performance.now();
    await callBridge(ctx, runtime("ses_role_hang"), "bash_status", { task_id: "t" });
    expect(performance.now() - started).toBeLessThan(LOOKUP_TIMEOUT_MS + 500);
    expect(sends[0]).not.toHaveProperty("worker_session");
  });
});

describe("host tool names on every bridge request", () => {
  // OpenCode 2 registers AFT's bash and apply_patch as shell and patch; AFT's
  // reply text names them only if each request says so.
  test("an OpenCode 2 context sends its names on both bridge helpers", async () => {
    _resetSubagentCacheForTest();
    const { ctx, sends, toolCalls } = harness(undefined);
    const names = { bash: "shell", apply_patch: "patch" };
    ctx.hostToolNames = names;
    await callBridge(ctx, runtime("ses_names_v2"), "bash", { command: "ls" });
    await callToolCall(ctx, runtime("ses_names_v2"), "edit", { patch: "p" });
    expect(sends[0]).toMatchObject({ session_id: "ses_names_v2", host_tool_names: names });
    expect(toolCalls[0]?.hostToolNames).toEqual(names);
  });

  test("a context without names sends none", async () => {
    _resetSubagentCacheForTest();
    const { ctx, sends, toolCalls } = harness(undefined);
    await callBridge(ctx, runtime("ses_names_v1"), "bash", { command: "ls" });
    await callToolCall(ctx, runtime("ses_names_v1"), "edit", { patch: "p" });
    expect(sends[0]).not.toHaveProperty("host_tool_names");
    expect(toolCalls[0]).not.toHaveProperty("hostToolNames");
  });

  test("a runon tool_call with a down module returns a safe typed refusal", async () => {
    _resetSubagentCacheForTest();
    const { ctx } = harness(
      undefined,
      false,
      new BridgeTransportUnavailableError("transport down"),
    );

    let refusal: unknown;
    try {
      await callToolCall(ctx, runtime("ses_remote_down"), "bash", {
        command: "uname -m",
        runon: "linux",
      });
    } catch (error) {
      refusal = error;
    }

    expect(refusal).toMatchObject({ code: "aft_module_unavailable", kind: "not_sent" });
    expect((refusal as Error).message).toContain("runon has no host fallback");
    expect((refusal as Error).message).toContain("safe to retry");
  });
});
