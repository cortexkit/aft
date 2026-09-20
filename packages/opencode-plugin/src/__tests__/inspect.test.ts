/// <reference path="../bun-test.d.ts" />

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtempSync, realpathSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import {
  createInspectTier2IdleScheduler,
  inspectTools,
  parseInspectTerminal,
  renderInspectTerminal,
  shouldRegisterInspectTool,
} from "../tools/inspect.js";
import type { PluginContext } from "../types.js";
import { noopAsk } from "./test-helpers";

let projectRoot: string;

beforeAll(() => {
  projectRoot = realpathSync(mkdtempSync(join(tmpdir(), "aft-test-repo-")));
});

afterAll(() => {
  rmSync(projectRoot, { recursive: true, force: true });
});

type BridgeResponse = Record<string, unknown>;
type ToolCallCall = {
  sessionId: string | undefined;
  name: string;
  rawArgs: Record<string, unknown>;
  options?: Record<string, unknown>;
};
type CapturedTimer = { callback: () => void; delay: number; cleared: boolean };

function createPluginContext(pool: BridgePool, config: Record<string, unknown>): PluginContext {
  return {
    pool,
    client: {
      lsp: { status: async () => ({ data: [] }) },
      find: { symbols: async () => ({ data: [] }) },
    },
    config: config as PluginContext["config"],
    storageDir: "/tmp/aft-test",
  };
}

function createMockSdkContext(directory = projectRoot): ToolContext {
  return {
    sessionID: "inspect-session",
    messageID: "message-id",
    agent: "test",
    directory,
    worktree: directory,
    abort: new AbortController().signal,
    metadata: () => {},
    ask: noopAsk,
  };
}

function schemaDescription(schema: unknown): string {
  const record = schema as { description?: string; _def?: { description?: string } };
  return record.description ?? record._def?.description ?? "";
}

function createInspectHarness(
  sendImpl: (
    name: string,
    args: Record<string, unknown>,
  ) => Promise<BridgeResponse> | BridgeResponse,
  config: Record<string, unknown> = {},
) {
  const toolCallCalls: ToolCallCall[] = [];
  const localBridge = {
    toolCall: async (
      sessionId: string | undefined,
      name: string,
      rawArgs: Record<string, unknown> = {},
      options?: Record<string, unknown>,
    ) => {
      toolCallCalls.push({ sessionId, name, rawArgs, options });
      return await sendImpl(name, rawArgs);
    },
  };
  const pool = { getBridge: () => localBridge } as unknown as BridgePool;
  return { toolCallCalls, tools: inspectTools(createPluginContext(pool, config)) };
}

function freshTerminal() {
  return {
    success: true,
    inspect_terminal: "fresh",
    text: "fresh result body",
    wait_stamp: {
      text: "waited: no; completed: stat_verification",
      phases: [{ id: "stat_verification", category: "duplicates" }],
    },
  };
}

describe("aft_inspect tool", () => {
  test("documents blocking-fresh results, scope narrowing, and the alert channel", () => {
    const { tools } = createInspectHarness(() => freshTerminal());
    const inspect = tools.aft_inspect;

    expect(inspect.description).toContain("Blocking-fresh");
    expect(inspect.description).toContain("wait-stamp");
    expect(inspect.description).toContain("alert channel");
    expect(inspect.description).not.toContain("short deadline");
    expect(inspect.description).not.toContain("pending_categories");
    expect(inspect.description).not.toContain("background warmup");
    expect(schemaDescription(inspect.args.scope)).toContain("one path, or an array of paths");
    expect(schemaDescription(inspect.args.scope)).toContain("not a space-separated list");
    expect(schemaDescription(inspect.args.scope)).toContain("`scope=` narrows results");
    expect(schemaDescription(inspect.args.scope)).not.toContain("Tier 1 scopes the scan");
  });

  test("parses shared entries for fresh, interrupted, and both failed forms", () => {
    const fresh = parseInspectTerminal(freshTerminal());
    const interrupted = parseInspectTerminal({
      inspect_terminal: "interrupted",
      completed_phases: [{ id: "lsp_quiescence", producer: "rust_analyzer" }],
    });
    const failed = parseInspectTerminal({
      inspect_terminal: "phase_failed",
      completed_phases: [{ id: "callgraph_ready", category: "dead_code" }],
      failed_phase: "callgraph_ready",
      category: "dead_code",
      failure_reason: "writer_lease_unavailable",
      failure_detail: "writer is busy",
    });
    const preflight = parseInspectTerminal({
      inspect_terminal: "phase_failed",
      completed_phases: [],
      failure_reason: "missing_executable",
    });

    expect(fresh?.phases[0]).toEqual({
      id: "stat_verification",
      producer: undefined,
      category: "duplicates",
      alsoSatisfied: [],
    });
    expect(interrupted?.phases[0]).toMatchObject({
      id: "lsp_quiescence",
      producer: "rust_analyzer",
    });
    expect(failed).toMatchObject({
      failedPhase: { id: "callgraph_ready", category: "dead_code" },
      failureReason: "writer_lease_unavailable",
      failureDetail: "writer is busy",
    });
    expect(preflight).toMatchObject({
      failedPhase: undefined,
      failureReason: "missing_executable",
    });
    expect(renderInspectTerminal(preflight!)).toContain("(missing_executable)");
  });

  test("renders phase failures with detail and bounded retry guidance", () => {
    const terminal = parseInspectTerminal({
      inspect_terminal: "phase_failed",
      completed_phases: [{ id: "stat_verification" }],
      failure_reason: "inspect_not_fresh",
      failure_detail: "metrics did not complete",
    });

    const rendered = renderInspectTerminal(terminal!, "request failed");
    expect(rendered).toContain(
      "inspect could not complete: metrics did not complete (inspect_not_fresh).",
    );
    expect(rendered).toContain("Completed phases: 1. Retry, or narrow with sections=...");
    expect(rendered).not.toContain("request failed");
  });

  test("renders interrupted terminals with safe retry guidance", () => {
    const terminal = parseInspectTerminal({
      inspect_terminal: "interrupted",
      completed_phases: [{ id: "lsp_quiescence" }],
    });

    const rendered = renderInspectTerminal(terminal!, "request failed");
    expect(rendered).toContain("inspect was interrupted");
    expect(rendered).toContain("Retry is safe");
    expect(rendered).not.toContain("request failed");
  });

  test("returns exactly one terminal result and no inspect follow-up", async () => {
    for (const response of [
      freshTerminal(),
      {
        success: false,
        inspect_terminal: "interrupted",
        text: "interrupted result body",
        completed_phases: [{ id: "lsp_start", producer: "tsserver" }],
      },
      {
        success: false,
        inspect_terminal: "phase_failed",
        text: "failure result body",
        completed_phases: [{ id: "tier2_rescan", category: "dead_code" }],
        failed_phase: "tier2_rescan",
        category: "dead_code",
        failure_reason: "tier2_rescan_errored",
      },
      {
        success: false,
        inspect_terminal: "phase_failed",
        text: "preflight result body",
        completed_phases: [],
        failure_reason: "root_resolution_failed",
      },
    ]) {
      const { toolCallCalls, tools } = createInspectHarness(() => response);
      const result = await tools.aft_inspect.execute({}, createMockSdkContext());

      const terminal = response.inspect_terminal.toUpperCase().replaceAll("_", "-");
      if (terminal === "INTERRUPTED") {
        expect(result).toContain("inspect was interrupted");
      } else if (terminal === "PHASE-FAILED") {
        expect(result).toContain("inspect could not complete");
      } else {
        expect(result).toContain(terminal);
      }
      expect(toolCallCalls).toHaveLength(1);
      expect(toolCallCalls[0]).toMatchObject({ name: "inspect" });
      expect(toolCallCalls[0]?.options).not.toHaveProperty("keepBridgeOnTimeout");
    }
  });

  test("does not retry after a transport error", async () => {
    const { toolCallCalls, tools } = createInspectHarness(() => {
      throw new Error("transport unavailable");
    });

    await expect(tools.aft_inspect.execute({}, createMockSdkContext())).rejects.toThrow(
      "transport unavailable",
    );
    expect(toolCallCalls).toHaveLength(1);
  });

  test("uses the default diagnostics deadline plus transport headroom", async () => {
    const { toolCallCalls, tools } = createInspectHarness(() => freshTerminal());
    await tools.aft_inspect.execute({}, createMockSdkContext(projectRoot));

    expect(toolCallCalls[0]?.options).toMatchObject({ transportTimeoutMs: 150_000 });
  });

  test("sends explicit inspect arguments with the configured diagnostics budget", async () => {
    const { toolCallCalls, tools } = createInspectHarness(() => freshTerminal(), {
      inspect: { diagnostics_timeout_ms: 180_000 },
    });
    await tools.aft_inspect.execute(
      { sections: ["todos", "dead_code"], scope: "src", topK: 7 },
      createMockSdkContext(projectRoot),
    );

    expect(toolCallCalls).toHaveLength(1);
    expect(toolCallCalls[0]).toMatchObject({
      sessionId: "inspect-session",
      name: "inspect",
      rawArgs: { sections: ["todos", "dead_code"], scope: join(projectRoot, "src"), topK: 7 },
      options: { transportTimeoutMs: 210_000 },
    });
    expect(toolCallCalls[0]?.options).not.toHaveProperty("keepBridgeOnTimeout");
  });

  test("registration gate follows surface, disabled_tools, and inspect.enabled", () => {
    expect(shouldRegisterInspectTool({ tool_surface: "recommended" })).toBe(true);
    expect(shouldRegisterInspectTool({ tool_surface: "minimal" })).toBe(false);
    expect(shouldRegisterInspectTool({ disabled_tools: ["aft_inspect"] })).toBe(false);
    expect(shouldRegisterInspectTool({ inspect: { enabled: false } })).toBe(false);
  });

  test("session idle scheduling remains separate from an inspect terminal", async () => {
    const timers: CapturedTimer[] = [];
    const runs: string[] = [];
    const scheduler = createInspectTier2IdleScheduler({
      isEnabled: () => true,
      idleMinutes: () => 4,
      run: async (sessionID) => {
        runs.push(sessionID);
      },
      setTimer: (callback, delay) => {
        const timer = { callback, delay, cleared: false };
        timers.push(timer);
        return timer as unknown as ReturnType<typeof setTimeout>;
      },
      clearTimer: (timer) => {
        (timer as unknown as CapturedTimer).cleared = true;
      },
    });

    scheduler.schedule("sid-1");
    expect(timers[0]?.delay).toBe(4 * 60 * 1000);
    timers[0]?.callback();
    await Promise.resolve();
    expect(runs).toEqual(["sid-1"]);
  });
});
