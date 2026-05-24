/// <reference path="../bun-test.d.ts" />
import { describe, expect, mock, test } from "bun:test";
import { resolve } from "node:path";
import type { BridgePool, BridgeRequestOptions } from "@cortexkit/aft-bridge";
import { type ToolContext, tool } from "@opencode-ai/plugin";
import { consumeToolMetadata } from "../metadata-store.js";
import { _resetSubagentCacheForTest } from "../shared/subagent-detect.js";
import { createBashKillTool, createBashStatusTool, createBashTool } from "../tools/bash.js";
import type { PluginContext } from "../types.js";
import { mockAsk, noopAsk } from "./test-helpers";

const PROJECT_CWD = resolve(import.meta.dir, "../../../..");

type BridgeResponse = Record<string, unknown>;
type SendCall = {
  command: string;
  params: Record<string, unknown>;
  options?: BridgeRequestOptions;
};
type ProgressHandler = (frame: { text: string }) => void;
type SafeParseSchema = { safeParse: (value: unknown) => { success: boolean } };

function createMockClient(): any {
  return {
    lsp: { status: async () => ({ data: [] }) },
    find: { symbols: async () => ({ data: [] }) },
  };
}

function createMockSdkContext(overrides: Partial<ToolContext> = {}): ToolContext {
  return {
    sessionID: "test-session",
    messageID: "test-message",
    agent: "test-agent",
    directory: PROJECT_CWD,
    worktree: PROJECT_CWD,
    abort: new AbortController().signal,
    metadata: () => {},
    ask: noopAsk,
    callID: "test-call",
    ...overrides,
  } as ToolContext;
}

function createHarness(
  sendImpl: (
    command: string,
    params: Record<string, unknown>,
    options?: BridgeRequestOptions & { onProgress?: ProgressHandler },
  ) => Promise<BridgeResponse> | BridgeResponse,
  triggerImpl?: PluginContext["plugin"],
) {
  const calls: SendCall[] = [];
  const bridge = {
    send: async (
      command: string,
      params: Record<string, unknown> = {},
      options?: BridgeRequestOptions & { onProgress?: ProgressHandler },
    ) => {
      calls.push({ command, params, options });
      return await sendImpl(command, params, options);
    },
  };
  const pool = { getBridge: () => bridge } as unknown as BridgePool;
  const ctx: PluginContext = {
    pool,
    client: createMockClient(),
    plugin: triggerImpl,
    config: {} as PluginContext["config"],
    storageDir: "/tmp/aft-test",
  };
  return { calls, tool: createBashTool(ctx) };
}

function safeParse(schema: unknown, value: unknown): { success: boolean } {
  return (schema as SafeParseSchema).safeParse(value);
}

describe("OpenCode bash adapter", () => {
  test("schema accepts valid unified bash params and rejects invalid shapes", () => {
    const { tool: bash } = createHarness(() => ({ success: true, output: "" }));

    expect(bash.description).toContain("By default, output is compressed");
    expect(bash.description).toContain("compressed: false");
    expect(bash.description).toContain("background: true");

    expect(safeParse(bash.args.command, "ls -la").success).toBe(true);
    expect(safeParse(bash.args.timeout, 120_000).success).toBe(true);
    expect(safeParse(bash.args.workdir, PROJECT_CWD).success).toBe(true);
    expect(safeParse(bash.args.description, "List files").success).toBe(true);
    expect(safeParse(bash.args.background, true).success).toBe(true);
    expect(safeParse(bash.args.compressed, false).success).toBe(true);

    expect(safeParse(bash.args.command, 123).success).toBe(false);
    expect(safeParse(bash.args.timeout, "slow").success).toBe(false);
    expect(safeParse(bash.args.background, "yes").success).toBe(false);
    expect(safeParse(bash.args.compressed, "no").success).toBe(false);

    for (const schema of Object.values(bash.args)) {
      const jsonSchema = tool.schema.toJSONSchema(schema) as { description?: string };
      expect(jsonSchema.description?.length).toBeGreaterThan(20);
    }
  });

  test("permission loop asks for each PermissionAsk and retries with permissions_granted", async () => {
    const ask = mockAsk();
    let sendCount = 0;
    const { calls, tool: bash } = createHarness((_command, _params, _options) => {
      sendCount++;
      if (sendCount === 1) {
        return {
          success: false,
          code: "permission_required",
          asks: [
            { kind: "bash", patterns: ["rm *"], always: ["rm *"] },
            { kind: "external_directory", patterns: ["/tmp/*"], always: [] },
          ],
        };
      }
      return { success: true, output: "ok", exit_code: 0, truncated: false };
    });

    await bash.execute({ command: "rm -rf /tmp/demo" }, createMockSdkContext({ ask }));

    expect(ask).toHaveBeenCalledTimes(2);
    expect(ask.mock.calls[0][0]).toEqual({
      permission: "bash",
      patterns: ["rm *"],
      always: ["rm *"],
      metadata: {},
    });
    expect(ask.mock.calls[1][0]).toEqual({
      permission: "external_directory",
      patterns: ["/tmp/*"],
      always: [],
      metadata: {},
    });
    expect(calls).toHaveLength(2);
    expect(calls[1].params.permissions_granted).toEqual(["rm *", "/tmp/*"]);
  });

  test("shell.env trigger fires before bridge call and merged env is forwarded", async () => {
    const events: string[] = [];
    const trigger = mock(async () => {
      events.push("trigger");
      return { env: { FOO: "bar", TOKEN: "redacted" } };
    });
    const { calls, tool: bash } = createHarness(
      () => {
        events.push("bridge");
        return { success: true, output: "env", exit_code: 0, truncated: false };
      },
      { trigger },
    );

    await bash.execute(
      { command: "printenv FOO", workdir: "/tmp/project" },
      createMockSdkContext({ sessionID: "s1", callID: "c1" } as Partial<ToolContext>),
    );

    expect(events).toEqual(["trigger", "bridge"]);
    expect(trigger).toHaveBeenCalledTimes(1);
    expect(trigger.mock.calls[0]).toEqual([
      "shell.env",
      { cwd: "/tmp/project", sessionID: "s1", callID: "c1" },
      { env: {} },
    ]);
    expect(calls[0].params.env).toEqual({ FOO: "bar", TOKEN: "redacted" });
  });

  test("transport timeout is bounded by wait-window, not user-supplied task budget", async () => {
    // After the v0.20+ foreground-as-polled-background architecture, the
    // Rust `bash` call returns a `running` status immediately — it does NOT
    // block until the child exits. The transport timeout therefore covers
    // only spawn + protocol round-trip, not the full task budget. A user
    // asking for `timeout: 600_000` (10-minute kill cap) still gets the
    // 30s baseline transport budget because the bridge call returns fast
    // and the long task survives in the background after promotion.
    const { calls, tool: bash } = createHarness(() => ({
      success: true,
      output: "built",
      exit_code: 0,
      truncated: false,
    }));

    await bash.execute({ command: "cargo build", timeout: 600_000 }, createMockSdkContext());

    expect(calls).toHaveLength(1);
    // The user's kill cap still propagates to Rust as the task timeout.
    expect(calls[0].params.timeout).toBe(600_000);
    // But transport timeout is the 30s baseline — wait-window (5s) plus
    // overhead (5s) is well below the floor.
    expect(calls[0].options?.transportTimeoutMs).toBe(30_000);
  });

  test("progress callback forwards rolling output previews through ctx.metadata", async () => {
    const metadata = mock(() => {});
    const { tool: bash } = createHarness((_command, _params, options) => {
      options?.onProgress?.({ text: "hello " });
      options?.onProgress?.({ text: "world" });
      return { success: true, output: "hello world", exit_code: 0, truncated: false };
    });

    await bash.execute(
      { command: "printf hello", description: "Print greeting" },
      createMockSdkContext({ metadata }),
    );

    expect(metadata.mock.calls[0][0]).toEqual({ output: "hello ", description: "Print greeting" });
    expect(metadata.mock.calls[1][0]).toEqual({
      output: "hello world",
      description: "Print greeting",
    });
    expect(metadata.mock.calls.at(-1)?.[0]).toEqual({
      output: "hello world",
      description: "Print greeting",
      exit: 0,
      truncated: false,
    });
  });

  test("bg_completions are captured for notification hooks, not appended by bash adapter", async () => {
    const { tool: bash } = createHarness(() => ({
      success: true,
      output: "foreground",
      exit_code: 0,
      truncated: false,
      bg_completions: [
        { task_id: "abc123", status: "completed", exit_code: 0, command: "sleep 1; echo done" },
        { task_id: "xyz456", status: "killed", exit_code: null, command: "long-running script" },
      ],
    }));

    const output = await bash.execute({ command: "echo foreground" }, createMockSdkContext());

    expect(output).toBe("foreground");
  });

  test("truncation pointer and exit code are appended to agent-visible output, full payload stored as metadata", async () => {
    const { tool: bash } = createHarness(() => ({
      success: true,
      output: "done",
      exit_code: 0,
      truncated: true,
      output_path: "/tmp/bash-output.txt",
    }));

    const output = await bash.execute(
      { command: "echo done", description: "Echo done" },
      createMockSdkContext({
        sessionID: "meta-session",
        callID: "meta-call",
      } as Partial<ToolContext>),
    );
    const stored = consumeToolMetadata("meta-session", "meta-call");

    // Truncation must be visible to the agent (so it knows full output is on
    // disk); metadata payload preserves the structured fields for the UI.
    expect(output).toBe("done\n[output truncated; full output at /tmp/bash-output.txt]");
    expect(stored).toEqual({
      title: "Echo done",
      metadata: {
        description: "Echo done",
        output: "done",
        exit: 0,
        truncated: true,
        outputPath: "/tmp/bash-output.txt",
      },
    });
  });

  test("non-zero exit code is appended to agent-visible output", async () => {
    const { tool: bash } = createHarness(() => ({
      success: true,
      output: "command failed\n",
      exit_code: 2,
      truncated: false,
    }));

    const output = await bash.execute({ command: "false" }, createMockSdkContext());

    expect(output).toBe("command failed\n\n[exit code: 2]");
  });

  test("background spawn returns a concise started line and stores task metadata", async () => {
    const { tool: bash } = createHarness(() => ({
      success: true,
      status: "running",
      task_id: "task-xyz",
    }));

    const output = await bash.execute(
      { command: "sleep 30 && echo done", background: true },
      createMockSdkContext({
        sessionID: "bg-session",
        callID: "bg-call",
      } as Partial<ToolContext>),
    );
    const stored = consumeToolMetadata("bg-session", "bg-call");

    // The "completion reminder" sentence is load-bearing — it tells the
    // agent the notification mechanism exists so it stops polling. Don't
    // soften this assertion; if the wording changes accidentally we want
    // the test to fail.
    expect(output).toBe(
      "Background task started: task-xyz. A completion reminder will be delivered automatically; don't poll bash_status.",
    );
    expect(stored?.metadata).toEqual({
      description: undefined,
      output:
        "Background task started: task-xyz. A completion reminder will be delivered automatically; don't poll bash_status.",
      status: "running",
      taskId: "task-xyz",
    });
  });

  test("foreground running task polls to terminal status and returns inline output", async () => {
    const { calls, tool: bash } = createHarness((command) => {
      if (command === "bash") return { success: true, status: "running", task_id: "task-inline" };
      return {
        success: true,
        status: "completed",
        exit_code: 0,
        duration_ms: 100,
        output_preview: "done",
        output_truncated: false,
      };
    });

    const output = await bash.execute({ command: "printf done" }, createMockSdkContext());

    expect(output).toBe("done");
    expect(calls.map((call) => call.command)).toEqual(["bash", "bash_status"]);
    expect(calls[0].params.notify_on_completion).toBe(false);
  });

  test("foreground running task promotes to background after wait timeout", async () => {
    const { calls, tool: bash } = createHarness((command) => {
      if (command === "bash") return { success: true, status: "running", task_id: "task-promote" };
      if (command === "bash_status") return { success: true, status: "running" };
      return { success: true, task_id: "task-promote", promoted: true };
    });

    const output = await bash.execute(
      { command: "sleep 2", timeout: 0 },
      createMockSdkContext({ sessionID: "promote-session" }),
    );

    expect(output).toContain("promoted to background: task-promote");
    expect(calls.map((call) => call.command)).toEqual(["bash", "bash_status", "bash_promote"]);
  });

  test("explicit background spawn enables completion notifications", async () => {
    const { calls, tool: bash } = createHarness(() => ({
      success: true,
      status: "running",
      task_id: "task-notify",
    }));

    const output = await bash.execute(
      { command: "sleep 30", background: true },
      createMockSdkContext(),
    );

    expect(output).toContain("Background task started: task-notify");
    expect(calls).toHaveLength(1);
    expect(calls[0].params.notify_on_completion).toBe(true);
  });
});

describe("bash_status tool", () => {
  function makeCtx(sendImpl: (cmd: string, params: Record<string, unknown>) => BridgeResponse) {
    const bridge = {
      send: async (cmd: string, params: Record<string, unknown> = {}) => sendImpl(cmd, params),
    };
    const pool = { getBridge: () => bridge } as unknown as BridgePool;
    const ctx: PluginContext = {
      pool,
      client: createMockClient(),
      config: {} as PluginContext["config"],
      storageDir: "/tmp/aft-test",
    };
    return { ctx, statusTool: createBashStatusTool(ctx), killTool: createBashKillTool(ctx) };
  }

  test("returns running status with anti-polling reminder, no output preview", async () => {
    const { statusTool } = makeCtx((_cmd, _params) => ({
      success: true,
      status: "running",
      exit_code: null,
      duration_ms: 3000,
      output_preview: null,
    }));
    const result = await statusTool.execute({ taskId: "bash-abc123" }, createMockSdkContext());
    // Header line preserved.
    expect(result).toContain("Task bash-abc123: running 3s");
    // Anti-polling reminder appended to running tasks. Same wording as the
    // initial spawn line so the agent sees consistent guidance.
    expect(result).toContain("A completion reminder will be delivered automatically; don't poll.");
    expect(result).not.toContain("null");
  });

  test("completed status renders preview without anti-polling suffix", async () => {
    const { statusTool } = makeCtx((_cmd, _params) => ({
      success: true,
      status: "completed",
      exit_code: 0,
      duration_ms: 15168,
      output_preview: "test 1: bg starting at 09:19:24\ntest 1: bg done at 09:19:39",
    }));
    const result = await statusTool.execute({ taskId: "bash-6b454047" }, createMockSdkContext());
    expect(result).toContain("Task bash-6b454047: completed (exit 0) 15s");
    expect(result).toContain("test 1: bg starting at");
    expect(result).toContain("test 1: bg done at");
    // Terminal statuses must NOT carry the anti-polling reminder — agent is
    // already consuming the result and shouldn't get noise.
    expect(result).not.toContain("don't poll");
  });

  test("failed/killed/timed_out terminal statuses do not append anti-polling reminder", async () => {
    for (const status of ["failed", "killed", "timed_out"] as const) {
      const { statusTool } = makeCtx((_cmd, _params) => ({
        success: true,
        status,
        exit_code: status === "killed" ? null : 1,
        duration_ms: 5000,
      }));
      const result = await statusTool.execute({ taskId: "bash-end" }, createMockSdkContext());
      expect(result).not.toContain("don't poll");
    }
  });

  test("forwards task_id as snake_case to bridge", async () => {
    const calls: Array<{ cmd: string; params: Record<string, unknown> }> = [];
    const { statusTool } = makeCtx((cmd, params) => {
      calls.push({ cmd, params });
      return { success: true, status: "running", exit_code: null, duration_ms: 0 };
    });
    await statusTool.execute({ taskId: "bash-deadbeef" }, createMockSdkContext());
    expect(calls[0].cmd).toBe("bash_status");
    expect(calls[0].params.task_id).toBe("bash-deadbeef");
  });

  test("throws on bridge error", async () => {
    const { statusTool } = makeCtx(() => ({
      success: false,
      code: "not_found",
      message: "task bash-unknown not found",
    }));
    await expect(
      statusTool.execute({ taskId: "bash-unknown" }, createMockSdkContext()),
    ).rejects.toThrow("task bash-unknown not found");
  });

  test("bash_kill forwards task_id and returns confirmation", async () => {
    const calls: Array<{ cmd: string; params: Record<string, unknown> }> = [];
    const { killTool } = makeCtx((cmd, params) => {
      calls.push({ cmd, params });
      return { success: true, status: "killed" };
    });
    const result = await killTool.execute({ taskId: "bash-deadbeef" }, createMockSdkContext());
    expect(result).toBe("Task bash-deadbeef: killed");
    expect(calls[0].cmd).toBe("bash_kill");
    expect(calls[0].params.task_id).toBe("bash-deadbeef");
  });

  test("bash_kill surfaces already-terminal status from bridge", async () => {
    const { killTool } = makeCtx(() => ({ success: true, status: "completed", exit_code: 0 }));
    const result = await killTool.execute({ taskId: "bash-done" }, createMockSdkContext());
    expect(result).toBe("Task bash-done: completed");
  });

  test("bash_kill throws on bridge error", async () => {
    const { killTool } = makeCtx(() => ({
      success: false,
      code: "not_running",
      message: "task already finished",
    }));
    await expect(killTool.execute({ taskId: "bash-done" }, createMockSdkContext())).rejects.toThrow(
      "task already finished",
    );
  });
});

// =============================================================================
// Subagent gating: AFT bash auto-promotes >5s tasks to background, which kills
// subagents waiting for the completion reminder. The bash tool detects
// subagent sessions (via client.session.get parentID) and:
//   1. Silently converts `background: true` to `background: false` — the
//      task_id the subagent would otherwise receive is unreachable because
//      the subagent terminates after its single response, so we run the
//      command inline instead. The subagent gets actual output, not a dead
//      task_id.
//   2. Extends the foreground poll window to the task's full hard-kill timeout
//      so the bash call stays inline until terminal regardless of duration.
// =============================================================================

function createSubagentClient(parentID: string = "ses_parent_xyz"): any {
  return {
    lsp: { status: async () => ({ data: [] }) },
    find: { symbols: async () => ({ data: [] }) },
    session: {
      // Real SDK shape: { path: { id }, query?: { directory } }.
      get: async (input: { path: { id: string } }) => ({
        data: { id: input.path.id, parentID },
      }),
    },
  };
}

function createSubagentHarness(
  sendImpl: (
    command: string,
    params: Record<string, unknown>,
    options?: BridgeRequestOptions & { onProgress?: ProgressHandler },
  ) => Promise<BridgeResponse> | BridgeResponse,
  parentID?: string,
) {
  const calls: SendCall[] = [];
  const bridge = {
    send: async (
      command: string,
      params: Record<string, unknown> = {},
      options?: BridgeRequestOptions & { onProgress?: ProgressHandler },
    ) => {
      calls.push({ command, params, options });
      return await sendImpl(command, params, options);
    },
  };
  const pool = { getBridge: () => bridge } as unknown as BridgePool;
  const ctx: PluginContext = {
    pool,
    client: createSubagentClient(parentID),
    plugin: undefined,
    config: {} as PluginContext["config"],
    storageDir: "/tmp/aft-test",
  };
  return { calls, tool: createBashTool(ctx) };
}

describe("OpenCode bash adapter — subagent gating", () => {
  test("subagent + background: true is silently converted to foreground (bridge sees background=false)", async () => {
    _resetSubagentCacheForTest();
    // Simulate a task that completes on the 2nd bash_status poll.
    let statusCalls = 0;
    const { calls, tool: bash } = createSubagentHarness((command) => {
      if (command === "bash") return { success: true, status: "running", task_id: "bash-conv" };
      if (command === "bash_status") {
        statusCalls += 1;
        if (statusCalls < 2) return { success: true, status: "running" };
        return {
          success: true,
          status: "completed",
          exit_code: 0,
          output_preview: "converted output",
          output_truncated: false,
        };
      }
      return { success: true };
    });
    const result = await bash.execute(
      { command: "sleep 30", background: true, timeout: 30_000 },
      createMockSdkContext({ sessionID: "ses_subagent_a" }),
    );
    // Result should be the actual command output, NOT a JSON refusal envelope
    // and NOT a "Background task started" launch line.
    expect(typeof result).toBe("string");
    expect(result as string).toContain("converted output");
    expect(result as string).not.toContain("Background task started");
    expect(result as string).not.toContain('"success":false');
    // The bridge MUST have been called with background=false (silent conversion).
    const bashCall = calls.find((c) => c.command === "bash");
    expect(bashCall).toBeDefined();
    expect(bashCall?.params.background).toBe(false);
    expect(bashCall?.params.notify_on_completion).toBe(false);
    // Subagents must never call bash_promote even when caller requested
    // background:true — the conversion happens upstream of promotion.
    expect(calls.find((c) => c.command === "bash_promote")).toBeUndefined();
  });

  test("subagent + foreground polls until terminal without promoting to background", async () => {
    _resetSubagentCacheForTest();
    // Simulate a task that completes on the 3rd bash_status poll (~300ms in).
    // Foreground primary sessions would promote at 5s; subagents must keep
    // polling until terminal regardless of duration.
    let statusCalls = 0;
    const { calls, tool: bash } = createSubagentHarness((command) => {
      if (command === "bash") return { success: true, status: "running", task_id: "bash-sub" };
      if (command === "bash_status") {
        statusCalls += 1;
        if (statusCalls < 3) return { success: true, status: "running" };
        return {
          success: true,
          status: "completed",
          exit_code: 0,
          output: "ok",
          truncated: false,
        };
      }
      return { success: true };
    });
    const result = await bash.execute(
      { command: "fast-test", timeout: 30_000 },
      createMockSdkContext({ sessionID: "ses_subagent_b" }),
    );
    expect(typeof result).toBe("string");
    expect(result as string).not.toContain("promoted to background");
    // bash_status should have been polled until terminal
    expect(calls.filter((c) => c.command === "bash_status").length).toBeGreaterThanOrEqual(3);
    // bash_promote should NEVER have been called for a subagent
    expect(calls.find((c) => c.command === "bash_promote")).toBeUndefined();
  });

  test("subagent + foreground without explicit timeout uses 30-minute default poll window", async () => {
    _resetSubagentCacheForTest();
    // We can't actually wait 30 minutes, but we can verify the code path
    // does NOT call bash_promote when the task is still running and no
    // explicit timeout was passed. (Test runs a fast termination so the
    // wait window is never hit.)
    const { calls, tool: bash } = createSubagentHarness((command) => {
      if (command === "bash") return { success: true, status: "running", task_id: "bash-sub2" };
      if (command === "bash_status") {
        return { success: true, status: "completed", exit_code: 0, output: "ok", truncated: false };
      }
      return { success: true };
    });
    await bash.execute(
      { command: "fast-test" }, // no timeout — should use DEFAULT_HARD_TIMEOUT_MS
      createMockSdkContext({ sessionID: "ses_subagent_c" }),
    );
    expect(calls.find((c) => c.command === "bash_promote")).toBeUndefined();
  });

  test("primary session + background: true still works (regression check)", async () => {
    _resetSubagentCacheForTest();
    // No client.session.get → resolveIsSubagent returns false → primary path.
    const { calls, tool: bash } = createHarness((command) => {
      if (command === "bash") return { success: true, status: "running", task_id: "bash-bg" };
      return { success: true };
    });
    const result = await bash.execute(
      { command: "sleep 30", background: true },
      createMockSdkContext({ sessionID: "ses_primary_a" }),
    );
    expect(typeof result).toBe("string");
    // Primary should NOT get the subagent error envelope
    expect(result as string).not.toContain("not allowed for subagents");
    // Primary background: true returns the launch line
    expect(result as string).toContain("bash-bg");
    expect(calls.find((c) => c.command === "bash")).toBeDefined();
  });

  test("SDK error on session.get defaults to primary (no regression)", async () => {
    _resetSubagentCacheForTest();
    const ctx: PluginContext = {
      pool: {
        getBridge: () => ({
          send: async (command: string) => {
            if (command === "bash")
              return { success: true, status: "running", task_id: "bash-err" };
            return { success: true };
          },
        }),
      } as unknown as BridgePool,
      client: {
        lsp: { status: async () => ({ data: [] }) },
        find: { symbols: async () => ({ data: [] }) },
        session: {
          get: async () => {
            throw new Error("simulated SDK failure");
          },
        },
      } as any,
      plugin: undefined,
      config: {} as PluginContext["config"],
      storageDir: "/tmp/aft-test",
    };
    const bash = createBashTool(ctx);
    const result = await bash.execute(
      { command: "sleep 30", background: true },
      createMockSdkContext({ sessionID: "ses_err_a" }),
    );
    // SDK failed → defaulted to primary → background: true succeeded
    expect(result as string).not.toContain("not allowed for subagents");
    expect(result as string).toContain("bash-err");
  });
});
