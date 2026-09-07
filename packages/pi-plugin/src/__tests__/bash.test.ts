/**
 * Unit tests for the Pi bash tool adapter.
 *
 * Covers:
 * - Schema validation (required command, optional fields)
 * - BashSpawnHook invocation
 * - Progress callback handling
 * - background task metadata tracking
 */

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { appendFile, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  BASH_HOST_FALLBACK_BANNER,
  type BinaryBridge,
  BridgeTransportUnavailableError,
} from "@cortexkit/aft-bridge";
import type { ExtensionAPI, Theme } from "@earendil-works/pi-coding-agent";
import { Container, Text } from "@earendil-works/pi-tui";
import { __resetBgNotificationStateForTests, sessionBgStates } from "../bg-notifications.js";
import {
  __parseWaitPatternForTests,
  __trimWaitScanBufferForTests,
  createBashStatusTool,
  createBashWatchTool,
  registerBashTool,
} from "../tools/bash.js";
import type { PluginContext } from "../types.js";

let projectRoot: string;

async function addArtifactBytes(
  command: string,
  params: Record<string, unknown>,
  response: Record<string, unknown>,
): Promise<Record<string, unknown>> {
  if (command !== "bash_status" || response.success === false) return response;
  const data = { ...response };
  const attach = async (
    pathKey: string,
    offsetKey: string,
    chunkKey: string,
    nextKey: string,
    rawKey?: string,
  ) => {
    const path = data[pathKey];
    if (typeof path !== "string") return;
    const bytes = await readFile(path);
    const offset = Math.max(0, Number(params[offsetKey] ?? 0));
    data[chunkKey] = bytes.subarray(offset).toString("base64");
    data[nextKey] = bytes.length;
    if (rawKey && (params.output_mode === "raw" || params.output_mode === "both")) {
      data[rawKey] = bytes.toString("utf8");
    }
  };
  if (data.mode === "pty") {
    await attach(
      "output_path",
      "output_offset",
      "output_chunk_base64",
      "output_next_offset",
      "pty_raw",
    );
  } else {
    await attach("output_path", "output_offset", "output_chunk_base64", "output_next_offset");
    await attach("stderr_path", "stderr_offset", "stderr_chunk_base64", "stderr_next_offset");
  }
  return data;
}

beforeAll(() => {
  projectRoot = mkdtempSync(join(tmpdir(), "aft-test-repo-"));
});

afterAll(() => {
  rmSync(projectRoot, { recursive: true, force: true });
});

// Minimal mock types
interface MockToolDef {
  name: string;
  label: string;
  description: string;
  parameters: unknown;
  execute: (
    toolCallId: string,
    params: unknown,
    signal: AbortSignal | undefined,
    onUpdate: ((update: unknown) => void) | undefined,
    ctx: { cwd: string },
  ) => Promise<unknown>;
  renderCall?: (args: unknown, theme: Theme, context: unknown) => unknown;
  renderResult?: (result: unknown, options: unknown, theme: Theme, context: unknown) => unknown;
}

interface MockExtensionContext {
  cwd: string;
  hasUI: boolean;
  signal?: AbortSignal;
  ui?: {
    confirm: (
      title: string,
      message: string,
      options?: { signal?: AbortSignal },
    ) => Promise<boolean>;
  };
}

// Mock theme for renderer tests
const mockTheme: Theme = {
  fg: (color: string, text: string) => `[${color}]${text}[/${color}]`,
  bold: (text: string) => `**${text}**`,
} as unknown as Theme;

// Build a minimal mock ExtensionAPI that captures registered tools
function makeMockApi(tools: Map<string, MockToolDef>): ExtensionAPI {
  return {
    registerTool: (tool: MockToolDef) => {
      tools.set(tool.name, tool);
    },
  } as unknown as ExtensionAPI;
}

// Mock bridge that captures calls and returns configurable responses
function makeMockBridge(response: Record<string, unknown> = {}): BinaryBridge {
  const sendFn = async () => ({ success: true, ...response });
  return {
    send: sendFn,
  } as unknown as BinaryBridge;
}

// Trackable mock bridge for verifying calls
function makeTrackableMockBridge(response: Record<string, unknown> = {}): {
  bridge: BinaryBridge;
  calls: unknown[];
} {
  const calls: unknown[] = [];
  const bridge = {
    send: async (...args: unknown[]) => {
      calls.push(args);
      const [command, params = {}] = args as [string, Record<string, unknown>];
      return addArtifactBytes(command, params, { success: true, ...response });
    },
  } as unknown as BinaryBridge;
  return { bridge, calls };
}

// Mock plugin context
function makeMockContext(
  bridge: BinaryBridge,
  config: PluginContext["config"] = {} as PluginContext["config"],
): PluginContext {
  return {
    pool: {
      getBridge: () => bridge,
    } as unknown as PluginContext["pool"],
    config,
    storageDir: "/tmp/test",
  };
}

async function spill(contents: string): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "aft-pi-bash-status-test-"));
  const file = join(dir, "task.out");
  await writeFile(file, contents);
  return file;
}

async function spillPair(
  stdout: string,
  stderr: string,
): Promise<{ dir: string; stdoutPath: string; stderrPath: string }> {
  const dir = await mkdtemp(join(tmpdir(), "aft-pi-bash-status-test-"));
  const stdoutPath = join(dir, "task.out");
  const stderrPath = join(dir, "task.err");
  await writeFile(stdoutPath, stdout);
  await writeFile(stderrPath, stderr);
  return { dir, stdoutPath, stderrPath };
}

function toolText(result: unknown): string {
  return (result as { content: Array<{ type: string; text: string }> }).content[0].text;
}

describe("bash tool adapter", () => {
  test("regex wait patterns keep raw source without compiling JS RegExp", () => {
    const pattern = __parseWaitPatternForTests({ regex: "(" });

    expect(pattern).toEqual({ kind: "regex", source: "(" });
    expect("value" in pattern!).toBe(false);
  });

  test("regex watches retain at most a 64 KB rolling scan window", () => {
    const pattern = __parseWaitPatternForTests({ regex: "not-found" });
    expect(pattern).toBeDefined();
    const text = "x".repeat(80 * 1024);

    const trimmed = __trimWaitScanBufferForTests(text, 5, pattern!);

    expect(Buffer.byteLength(trimmed.text, "utf8")).toBeLessThanOrEqual(64 * 1024);
    expect(trimmed.baseOffset).toBe(5 + 16 * 1024);
  });

  test("schema has comprehensive descriptions", () => {
    const tools = new Map<string, MockToolDef>();
    const api = makeMockApi(tools);
    const mockBridge = makeMockBridge();
    const ctx = makeMockContext(mockBridge);

    registerBashTool(api, ctx);

    const bashTool = tools.get("bash");
    expect(bashTool).toBeDefined();

    // Tool description mentions compressed and background options, and the
    // background-management mechanism the agent actually uses to wait.
    expect(bashTool!.description).toContain("compressed");
    expect(bashTool!.description).toContain("Piped commands run verbatim");
    expect(bashTool!.description).toContain("background");
    expect(bashTool!.description).toContain("wait: true");
    expect(bashTool!.description).toContain("bash_watch");
    const properties = (bashTool!.parameters as { properties?: Record<string, unknown> })
      .properties;
    expect(properties?.wait).toBeDefined();
    expect(properties?.sandbox).toMatchObject({ const: "host" });
  });

  test("schema omits background and PTY params when bash.background is disabled", () => {
    const tools = new Map<string, MockToolDef>();
    const api = makeMockApi(tools);
    const mockBridge = makeMockBridge();
    const ctx = makeMockContext(mockBridge, {
      bash: { background: false },
    } as PluginContext["config"]);

    registerBashTool(api, ctx);

    const bashTool = tools.get("bash");
    expect(bashTool).toBeDefined();
    const properties = (bashTool!.parameters as { properties?: Record<string, unknown> })
      .properties;
    expect(Object.keys(properties ?? {})).toEqual([
      "command",
      "timeout",
      "workdir",
      "description",
      "wait",
      "sandbox",
      "compressed",
    ]);
    expect(properties?.background).toBeUndefined();
    expect(properties?.pty).toBeUndefined();
    expect(properties?.ptyRows).toBeUndefined();
    expect(properties?.ptyCols).toBeUndefined();
    expect(bashTool!.description).toContain("foreground to completion");
    expect(bashTool!.description).not.toContain("bash_status");
    expect(bashTool!.description).not.toContain("bash_kill");
    expect(bashTool!.description).not.toContain("bash_watch");
    expect(bashTool!.description).not.toContain("pty: true");
  });

  test("PowerShell registration routes through the unified bash command family", async () => {
    const tools = new Map<string, MockToolDef>();
    const api = makeMockApi(tools);
    const { bridge, calls } = makeTrackableMockBridge({ output: "ok", exit_code: 0 });
    const ctx = makeMockContext(bridge);

    registerBashTool(api, ctx, false, "powershell", false, "powershell");
    await tools
      .get("powershell")!
      .execute("test-call", { command: "Get-ChildItem" }, undefined, undefined, {
        cwd: projectRoot,
        hasUI: false,
      });

    const call = calls[0] as [string, Record<string, unknown>];
    expect(call[0]).toBe("powershell");
    expect(call[1]).toMatchObject({ command: "Get-ChildItem", shell: "powershell" });
  });

  test("execute calls bridge with correct parameters", async () => {
    const tools = new Map<string, MockToolDef>();
    const api = makeMockApi(tools);
    const { bridge, calls } = makeTrackableMockBridge({
      output: "hello world",
      exit_code: 0,
      duration_ms: 100,
    });
    const ctx = makeMockContext(bridge);

    registerBashTool(api, ctx);

    const bashTool = tools.get("bash")!;
    const extCtx: MockExtensionContext = { cwd: projectRoot, hasUI: false };

    const result = (await bashTool.execute(
      "test-call",
      { command: "echo hello" },
      undefined,
      undefined,
      extCtx,
    )) as { content: Array<{ type: string; text: string }>; details: Record<string, unknown> };

    // Verify bridge was called
    expect(calls.length).toBe(1);

    // Check the command parameter
    const callArgs = calls[0] as [string, Record<string, unknown>];
    expect(callArgs[0]).toBe("bash");
    expect(callArgs[1].command).toBe("echo hello");

    // Verify result structure
    expect(result.content[0].text).toBe("hello world");
    expect(result.details.exit_code).toBe(0);
    expect(result.details.duration_ms).toBe(100);
  });

  test("forwards piped commands unchanged and does not append strip notes", async () => {
    const tools = new Map<string, MockToolDef>();
    const api = makeMockApi(tools);
    const { bridge, calls } = makeTrackableMockBridge({
      output: "failure details",
      exit_code: 1,
      duration_ms: 100,
    });
    const ctx = makeMockContext(bridge);

    registerBashTool(api, ctx);

    const bashTool = tools.get("bash")!;
    const commands = [
      "bun test | grep fail",
      "cargo test | grep -v '^'",
      "cargo test | awk 'END{exit 1}'",
      "pytest -q | grep SENTINEL || exit 1",
    ];

    for (const command of commands) {
      const result = (await bashTool.execute("test-call", { command }, undefined, undefined, {
        cwd: projectRoot,
      })) as { content: Array<{ type: string; text: string }> };

      const callArgs = calls.at(-1) as [string, Record<string, unknown>];
      expect(callArgs[1].command).toBe(command);
      expect(result.content[0].text).toContain("failure details");
      expect(result.content[0].text).not.toContain("AFT dropped");
    }
  });

  test("keeps filter pipes when compressed:false", async () => {
    const tools = new Map<string, MockToolDef>();
    const api = makeMockApi(tools);
    const { bridge, calls } = makeTrackableMockBridge({ output: "raw", exit_code: 0 });
    const ctx = makeMockContext(bridge);

    registerBashTool(api, ctx);

    const bashTool = tools.get("bash")!;
    const result = (await bashTool.execute(
      "test-call",
      { command: "bun test | grep fail", compressed: false },
      undefined,
      undefined,
      { cwd: projectRoot },
    )) as { content: Array<{ type: string; text: string }> };

    const callArgs = calls[0] as [string, Record<string, unknown>];
    expect(callArgs[1].command).toBe("bun test | grep fail");
    expect(result.content[0].text).not.toContain("AFT dropped");
  });

  test('forwards string compressed "false" as boolean false', async () => {
    const tools = new Map<string, MockToolDef>();
    const api = makeMockApi(tools);
    const { bridge, calls } = makeTrackableMockBridge({ output: "raw", exit_code: 0 });
    const ctx = makeMockContext(bridge);

    registerBashTool(api, ctx);

    await tools
      .get("bash")!
      .execute("test-call", { command: "printf raw", compressed: "false" }, undefined, undefined, {
        cwd: projectRoot,
      });

    const callArgs = calls[0] as [string, Record<string, unknown>];
    expect(callArgs[1].compressed).toBe(false);
  });

  test("background bash forwards user kill cap and uses wait-aware transport budget", async () => {
    // The main bash call is handled through server orchestration, so the
    // transport budget comes from the foreground wait window plus margin even
    // when an explicit background call returns immediately. The user-supplied
    // 40s timeout is still sent as the child-process kill cap.
    const tools = new Map<string, MockToolDef>();
    const api = makeMockApi(tools);
    const { bridge, calls } = makeTrackableMockBridge({
      status: "running",
      task_id: "bash-123",
      duration_ms: 5,
      output:
        "Background task started: bash-123. A completion reminder will be delivered automatically; don't poll bash_status.",
    });
    const ctx = makeMockContext(bridge);

    registerBashTool(api, ctx);

    const bashTool = tools.get("bash")!;
    const result = (await bashTool.execute(
      "test-call",
      { command: "bun test", timeout: 40_000, background: true, compressed: false },
      undefined,
      undefined,
      { cwd: projectRoot },
    )) as { content: Array<{ text: string }>; details: Record<string, unknown> };

    const callArgs = calls[0] as [string, Record<string, unknown>, Record<string, unknown>];
    expect(callArgs[0]).toBe("bash");
    expect(callArgs[1]).toMatchObject({
      command: "bun test",
      timeout: 40_000,
      background: true,
      notify_on_completion: true,
      compressed: false,
    });
    // The transport timeout is the default foreground wait window (15s) plus a
    // 10s margin for transport finalization.
    expect(callArgs[2].transportTimeoutMs).toBe(25_000);
    expect(callArgs[2].keepBridgeOnTimeout).toBe(true);
    expect(result.content[0].text).toContain("Background task started: bash-123");
    expect(result.details.task_id).toBe("bash-123");
  });

  test("foreground command returns server-orchestrated inline output without client polling", async () => {
    const tools = new Map<string, MockToolDef>();
    const api = makeMockApi(tools);
    const calls: unknown[] = [];
    const bridge = {
      send: async (
        command: string,
        params: Record<string, unknown>,
        options?: Record<string, unknown>,
      ) => {
        calls.push([command, params, options]);
        return {
          success: true,
          status: "completed",
          task_id: "task-inline",
          exit_code: 0,
          duration_ms: 100,
          output: "done",
          truncated: false,
        };
      },
    } as unknown as BinaryBridge;
    const ctx = makeMockContext(bridge);

    registerBashTool(api, ctx);

    const bashTool = tools.get("bash")!;
    const result = (await bashTool.execute(
      "test-call",
      { command: "printf done" },
      undefined,
      undefined,
      { cwd: projectRoot },
    )) as { content: Array<{ text: string }> };

    expect(result.content[0].text).toBe("done");
    expect(calls.map((call) => (call as [string])[0])).toEqual(["bash"]);
    const bashCall = calls[0] as [string, Record<string, unknown>, Record<string, unknown>];
    expect(bashCall[1]).toMatchObject({
      notify_on_completion: false,
      foreground_orchestrate: true,
      block_to_completion: false,
    });
    expect(bashCall[2].keepBridgeOnTimeout).toBe(true);
    expect(bashCall[2].transportTimeoutMs).toBe(25_000);
  });

  test("wait true forwards foreground wait mode and scales transport timeout", async () => {
    const tools = new Map<string, MockToolDef>();
    const api = makeMockApi(tools);
    const calls: unknown[] = [];
    const bridge = {
      send: async (
        command: string,
        params: Record<string, unknown>,
        options?: Record<string, unknown>,
      ) => {
        calls.push([command, params, options]);
        return {
          success: true,
          status: "completed",
          task_id: "task-wait",
          exit_code: 0,
          duration_ms: 100,
          output: "waited",
          truncated: false,
        };
      },
    } as unknown as BinaryBridge;
    const ctx = makeMockContext(bridge);

    registerBashTool(api, ctx);

    const result = (await tools
      .get("bash")!
      .execute(
        "test-call",
        { command: "sleep 2", wait: "true", timeout: 250 },
        undefined,
        undefined,
        { cwd: projectRoot },
      )) as { content: Array<{ text: string }> };

    expect(result.content[0].text).toBe("waited");
    expect(calls.map((call) => (call as [string])[0])).toEqual(["bash"]);
    const bashCall = calls[0] as [string, Record<string, unknown>, Record<string, unknown>];
    expect(bashCall[1]).toMatchObject({
      wait: true,
      block_to_completion: true,
      timeout: 250,
      background: false,
      notify_on_completion: false,
    });
    expect(bashCall[2].transportTimeoutMs).toBe(10_250);
  });

  test("wait true rejects background and pty contradictions", async () => {
    const tools = new Map<string, MockToolDef>();
    const api = makeMockApi(tools);
    registerBashTool(api, makeMockContext(makeMockBridge()));
    const bashTool = tools.get("bash")!;

    await expect(
      bashTool.execute(
        "test-call",
        { command: "sleep 2", wait: true, background: true },
        undefined,
        undefined,
        { cwd: projectRoot },
      ),
    ).rejects.toThrow("wait:true cannot be used with background:true");
    await expect(
      bashTool.execute(
        "test-call",
        { command: "python", wait: true, pty: true },
        undefined,
        undefined,
        { cwd: projectRoot },
      ),
    ).rejects.toThrow("wait:true cannot be used with pty:true");
  });

  test("foreground leading grep appends aft_search hint", async () => {
    const tools = new Map<string, MockToolDef>();
    const api = makeMockApi(tools);
    const bridge = {
      send: async () => ({
        success: true,
        status: "completed",
        task_id: "task-grep",
        exit_code: 0,
        duration_ms: 100,
        output: "src/file.ts:1:x",
        truncated: false,
      }),
    } as unknown as BinaryBridge;
    const ctx = makeMockContext(bridge);

    registerBashTool(api, ctx, true);

    const result = (await tools
      .get("bash")!
      .execute("test-call", { command: 'grep -nE "x" src/' }, undefined, undefined, {
        cwd: projectRoot,
      })) as { content: Array<{ text: string }> };

    expect(result.content[0].text).toContain("src/file.ts:1:x");
    expect(result.content[0].text).toContain("DO NOT search code by running grep/rg in bash");
    expect(result.content[0].text).toContain("Use the `aft_search` tool instead");
  });

  test("foreground filtering grep does not append code-search hint", async () => {
    const tools = new Map<string, MockToolDef>();
    const api = makeMockApi(tools);
    const bridge = {
      send: async () => ({
        success: true,
        status: "completed",
        task_id: "task-filter",
        exit_code: 0,
        duration_ms: 100,
        output: "failure details",
        truncated: false,
      }),
    } as unknown as BinaryBridge;
    const ctx = makeMockContext(bridge);

    registerBashTool(api, ctx, true);

    const result = (await tools
      .get("bash")!
      .execute("test-call", { command: "bun test | grep fail" }, undefined, undefined, {
        cwd: projectRoot,
      })) as { content: Array<{ text: string }> };

    expect(result.content[0].text).toContain("failure details");
    expect(result.content[0].text).not.toContain("DO NOT search code by running grep/rg in bash");
  });

  test("foreground promotion returns server message without client poll/promote calls", async () => {
    const tools = new Map<string, MockToolDef>();
    const api = makeMockApi(tools);
    const calls: unknown[] = [];
    const bridge = {
      send: async (
        command: string,
        params: Record<string, unknown>,
        options?: Record<string, unknown>,
      ) => {
        calls.push([command, params, options]);
        return {
          success: true,
          status: "running",
          task_id: "task-promote",
          output: `Foreground bash didn't finish within 0.1s and was promoted to background: task-promote. A completion reminder will be delivered automatically; use bash_status({ taskId: "task-promote" }) to inspect output or bash_kill({ taskId: "task-promote" }) to terminate.`,
        };
      },
    } as unknown as BinaryBridge;
    const ctx = makeMockContext(bridge);

    registerBashTool(api, ctx);

    const bashTool = tools.get("bash")!;
    process.env.AFT_TEST_FOREGROUND_WAIT_MS = "50";
    let result: { content: Array<{ text: string }> };
    try {
      result = (await bashTool.execute("test-call", { command: "sleep 2" }, undefined, undefined, {
        cwd: projectRoot,
      })) as { content: Array<{ text: string }> };
    } finally {
      delete process.env.AFT_TEST_FOREGROUND_WAIT_MS;
    }

    expect(result.content[0].text).toContain("promoted to background: task-promote");
    expect(calls.map((call) => (call as [string])[0])).toEqual(["bash"]);
    const bashCall = calls[0] as [string, Record<string, unknown>, Record<string, unknown>];
    expect(bashCall[1].foreground_orchestrate).toBe(true);
    expect(bashCall[1].block_to_completion).toBe(false);
    expect(bashCall[2].keepBridgeOnTimeout).toBe(true);
    expect(bashCall[2].transportTimeoutMs).toBe(10_050);
  });

  test("background disabled foreground command is block-to-completion on the server", async () => {
    const tools = new Map<string, MockToolDef>();
    const api = makeMockApi(tools);
    const calls: unknown[] = [];
    const bridge = {
      send: async (
        command: string,
        params: Record<string, unknown>,
        options?: Record<string, unknown>,
      ) => {
        calls.push([command, params, options]);
        return {
          success: true,
          status: "completed",
          task_id: "task-no-bg",
          exit_code: 0,
          duration_ms: 125,
          output: "finished without background",
          truncated: false,
        };
      },
    } as unknown as BinaryBridge;
    const ctx = makeMockContext(bridge, { bash: { background: false } } as PluginContext["config"]);

    registerBashTool(api, ctx);

    const result = (await tools
      .get("bash")!
      .execute(
        "test-call",
        { command: "sleep 2", background: true, pty: true, timeout: 25 },
        undefined,
        undefined,
        { cwd: projectRoot },
      )) as { content: Array<{ text: string }> };

    expect(result.content[0].text).toBe("finished without background");
    expect(calls.map((call) => (call as [string])[0])).toEqual(["bash"]);
    expect(calls.find((call) => (call as [string])[0] === "bash_promote")).toBeUndefined();
    const bashCall = calls[0] as [string, Record<string, unknown>, Record<string, unknown>];
    const bashParams = bashCall[1];
    expect(bashParams.background).toBe(false);
    expect(bashParams.notify_on_completion).toBe(false);
    expect(bashParams.pty).toBe(false);
    expect(bashParams.timeout).toBe(25);
    expect(bashParams.block_to_completion).toBe(true);
    expect(bashCall[2].transportTimeoutMs).toBe(10_025);
  });

  test("async bash_watch registration does not add synthetic outstanding task", async () => {
    __resetBgNotificationStateForTests();
    const tools = new Map<string, MockToolDef>();
    const api = makeMockApi(tools);
    const bridge = {
      send: async (command: string) =>
        command === "bash_notify"
          ? { success: true, watch_id: "watch-1" }
          : { success: true, status: "completed", exit_code: 0 },
    } as unknown as BinaryBridge;
    registerBashTool(api, makeMockContext(bridge));

    await tools
      .get("bash_watch")!
      .execute(
        "call",
        { task_id: "bash-finished", pattern: "READY", background: true },
        undefined,
        undefined,
        { cwd: projectRoot, sessionManager: { getSessionId: () => "s-watch" } },
      );

    expect(sessionBgStates.get("s-watch")?.outstandingTaskIds.has("bash-finished")).toBe(false);
  });

  test('async bash_watch forwards string once "false" as boolean false', async () => {
    const tools = new Map<string, MockToolDef>();
    const api = makeMockApi(tools);
    const { bridge, calls } = makeTrackableMockBridge({ watch_id: "watch-sticky" });
    registerBashTool(api, makeMockContext(bridge));

    await tools
      .get("bash_watch")!
      .execute(
        "call",
        { task_id: "bash-sticky", pattern: "READY", background: true, once: "false" },
        undefined,
        undefined,
        { cwd: projectRoot },
      );

    const notifyCall = calls.find((call) => (call as [string])[0] === "bash_notify") as [
      string,
      Record<string, unknown>,
    ];
    expect(notifyCall[1].once).toBe(false);
  });

  test("BashSpawnHook modifies command before bridge call", async () => {
    const tools = new Map<string, MockToolDef>();

    // Create API with a BashSpawnHook
    const hookCalls: Array<{ command: string; cwd?: string }> = [];
    const apiWithHook = {
      registerTool: (tool: MockToolDef) => {
        tools.set(tool.name, tool);
      },
      getHook: (name: string) => {
        if (name === "bashSpawn") {
          return async (ctx: { command: string; cwd?: string }) => {
            hookCalls.push(ctx);
            return {
              command: `modified: ${ctx.command}`,
              cwd: "/modified/cwd",
              env: { TEST_VAR: "value" },
            };
          };
        }
        return undefined;
      },
    } as unknown as ExtensionAPI;

    const { bridge, calls } = makeTrackableMockBridge({ output: "result" });
    const ctx = makeMockContext(bridge);

    registerBashTool(apiWithHook, ctx);

    const bashTool = tools.get("bash")!;
    const extCtx: MockExtensionContext = { cwd: projectRoot, hasUI: false };

    await bashTool.execute(
      "test-call",
      { command: "original command", workdir: "/original" },
      undefined,
      undefined,
      extCtx,
    );

    // Verify hook was called with original params
    expect(hookCalls.length).toBe(1);
    expect(hookCalls[0].command).toBe("original command");
    expect(hookCalls[0].cwd).toBe("/original");

    // Verify bridge received modified params
    const callArgs = calls[0] as [string, Record<string, unknown>];
    expect(callArgs[1].command).toBe("modified: original command");
    expect(callArgs[1].workdir).toBe("/modified/cwd");
    expect(callArgs[1].env).toEqual({ TEST_VAR: "value" });
  });

  test("BashSpawnHook errors are surfaced", async () => {
    const tools = new Map<string, MockToolDef>();

    const apiWithFailingHook = {
      registerTool: (tool: MockToolDef) => {
        tools.set(tool.name, tool);
      },
      getHook: () => {
        return async () => {
          throw new Error("Hook failed: permission denied");
        };
      },
    } as unknown as ExtensionAPI;

    const mockBridge = makeMockBridge();
    const ctx = makeMockContext(mockBridge);

    registerBashTool(apiWithFailingHook, ctx);

    const bashTool = tools.get("bash")!;
    const extCtx: MockExtensionContext = { cwd: projectRoot, hasUI: false };

    await expect(
      bashTool.execute("test-call", { command: "echo test" }, undefined, undefined, extCtx),
    ).rejects.toThrow("Hook failed: permission denied");
  });

  test("gate-off preserves a transport-dead error unchanged", async () => {
    const tools = new Map<string, MockToolDef>();
    const original = new BridgeTransportUnavailableError("byte-identical transport error");
    const bridge = {
      send: async () => {
        throw original;
      },
    } as unknown as BinaryBridge;
    registerBashTool(
      makeMockApi(tools),
      makeMockContext(bridge, { bash: { host_fallback: false } } as PluginContext["config"]),
    );

    let captured: unknown;
    try {
      await tools
        .get("bash")!
        .execute("call", { command: "printf should-not-run" }, undefined, undefined, {
          cwd: projectRoot,
          hasUI: false,
        });
    } catch (error) {
      captured = error;
    }

    expect(captured).toBe(original);
    expect((captured as Error).message).toBe("byte-identical transport error");
  });

  test("engine-alive errors never engage host fallback", async () => {
    const tools = new Map<string, MockToolDef>();
    const confirmations: unknown[] = [];
    const bridge = {
      send: async () => ({
        success: false,
        code: "invalid_request",
        message: "invalid request from engine",
      }),
    } as unknown as BinaryBridge;
    registerBashTool(
      makeMockApi(tools),
      makeMockContext(bridge, { bash: { host_fallback: true } } as PluginContext["config"]),
    );

    await expect(
      tools
        .get("bash")!
        .execute("call", { command: "printf should-not-run" }, undefined, undefined, {
          cwd: projectRoot,
          hasUI: true,
          ui: {
            confirm: async (...args) => {
              confirmations.push(args);
              return true;
            },
          },
        }),
    ).rejects.toThrow("invalid request from engine");
    expect(confirmations).toHaveLength(0);
  });

  // Spawns a real host process through the fallback executor; the first shell
  // exec on a cold CI runner takes seconds, so this budget is explicit rather
  // than Bun's 5s default.
  test("transport-dead fallback recovers on the next successful module call", async () => {
    const tools = new Map<string, MockToolDef>();
    const command =
      process.platform === "win32"
        ? `${JSON.stringify(process.execPath)} -e "process.stdout.write('pi-fallback')"`
        : "printf pi-fallback";
    let attempts = 0;
    const bridge = {
      send: async () => {
        attempts += 1;
        if (attempts === 1) throw new BridgeTransportUnavailableError("bridge spawn failed");
        return { success: true, output: "module-recovered", exit_code: 0, truncated: false };
      },
    } as unknown as BinaryBridge;
    registerBashTool(
      makeMockApi(tools),
      makeMockContext(bridge, { bash: { host_fallback: true } } as PluginContext["config"]),
    );
    const confirmations: Array<{ title: string; message: string }> = [];

    const result = await tools.get("bash")!.execute("call", { command }, undefined, undefined, {
      cwd: projectRoot,
      hasUI: true,
      ui: {
        confirm: async (title, message) => {
          confirmations.push({ title, message });
          return true;
        },
      },
    });
    const output = toolText(result);
    const recovered = toolText(
      await tools
        .get("bash")!
        .execute("recovery-call", { command: "printf module-recovered" }, undefined, undefined, {
          cwd: projectRoot,
          hasUI: true,
        }),
    );

    expect(output).toStartWith(`${BASH_HOST_FALLBACK_BANNER}\n`);
    expect(output).toContain("pi-fallback");
    expect(output).toEndWith("[exit code: 0]");
    expect(recovered).toBe("module-recovered");
    expect(recovered).not.toContain(BASH_HOST_FALLBACK_BANNER);
    expect(attempts).toBe(2);
    expect(confirmations).toEqual([
      {
        title: "AFT unavailable — run command on host?",
        message: `AFT UNAVAILABLE - host fallback execution:\n\nExact command:\n${command}\n\nWorking directory:\n${projectRoot}`,
      },
    ]);
  }, 20_000);

  test("execute throws Rust-side bash error responses", async () => {
    const tools = new Map<string, MockToolDef>();
    const api = makeMockApi(tools);
    const mockBridge = makeMockBridge({
      success: false,
      code: "execution_failed",
      message: "kaboom",
    });
    const ctx = makeMockContext(mockBridge);

    registerBashTool(api, ctx);

    const bashTool = tools.get("bash")!;
    const extCtx: MockExtensionContext = { cwd: projectRoot, hasUI: false };

    await expect(
      bashTool.execute("test-call", { command: "boom" }, undefined, undefined, extCtx),
    ).rejects.toThrow("kaboom");
  });

  test("progress callback streams output", async () => {
    const tools = new Map<string, MockToolDef>();
    const api = makeMockApi(tools);

    // Track progress callbacks
    const progressCallbacks: Array<{ text: string }> = [];

    // Bridge that simulates progress callbacks
    // callBridge passes options as 3rd argument to bridge.send
    const mockBridge = {
      send: async (
        _cmd: string,
        _params: unknown,
        options?: { onProgress?: (chunk: { kind: string; text: string }) => void },
      ) => {
        // Simulate progress
        if (options?.onProgress) {
          options.onProgress({ kind: "stdout", text: "line1\n" });
          options.onProgress({ kind: "stdout", text: "line2\n" });
          progressCallbacks.push({ text: "line1\n" }, { text: "line2\n" });
        }
        return { success: true, output: "final output", exit_code: 0 };
      },
    } as unknown as BinaryBridge;

    const ctx = makeMockContext(mockBridge);
    registerBashTool(api, ctx);

    const bashTool = tools.get("bash")!;
    const extCtx: MockExtensionContext = { cwd: projectRoot, hasUI: false };

    const updates: unknown[] = [];
    const result = await bashTool.execute(
      "test-call",
      { command: "long running" },
      undefined,
      (update) => updates.push(update),
      extCtx,
    );

    // Verify progress callbacks were invoked
    expect(progressCallbacks.length).toBe(2);

    // Verify final result has the output
    const finalResult = result as { content: Array<{ text: string }> };
    expect(finalResult.content[0].text).toContain("final output");
  });

  test("bg_completions metadata is not appended by bash adapter", async () => {
    const tools = new Map<string, MockToolDef>();
    const api = makeMockApi(tools);
    const mockBridge = makeMockBridge({
      output: "Main output",
      exit_code: 0,
      bg_completions: [
        { task_id: "bg-1", status: "completed", exit_code: 0, command: "npm install" },
        { task_id: "bg-2", status: "failed", exit_code: 1, command: "npm run build" },
      ],
    });
    const ctx = makeMockContext(mockBridge);

    registerBashTool(api, ctx);

    const bashTool = tools.get("bash")!;
    const extCtx: MockExtensionContext = { cwd: projectRoot, hasUI: false };

    const result = (await bashTool.execute(
      "test-call",
      { command: "main command" },
      undefined,
      undefined,
      extCtx,
    )) as {
      content: Array<{ type: string; text: string }>;
      details: { bg_completions?: Array<{ task_id: string }> };
    };

    expect(result.details.bg_completions).toBeUndefined();
    expect(result.content[0].text).toBe("Main output");
  });

  test("host escalation prompt shows exact payload and retry carries opaque grant", async () => {
    const tools = new Map<string, MockToolDef>();
    const api = makeMockApi(tools);
    const calls: Array<Record<string, unknown>> = [];
    const command = "printf 'exact  value'\nprintf done";
    const cwd = "/tmp/exact cwd";
    const bridge = {
      send: async (_name: string, params: Record<string, unknown>) => {
        calls.push(params);
        if (calls.length === 1) {
          return {
            success: false,
            code: "permission_required",
            message: "approval required",
            asks: [
              {
                kind: "escalation",
                command,
                cwd,
                grant_id: "esc_server_minted",
              },
            ],
          };
        }
        return { success: true, output: "approved", exit_code: 0 };
      },
    } as unknown as BinaryBridge;
    const confirmations: Array<{ title: string; message: string }> = [];
    const extCtx: MockExtensionContext = {
      cwd: projectRoot,
      hasUI: true,
      ui: {
        confirm: async (title, message) => {
          confirmations.push({ title, message });
          return true;
        },
      },
    };

    registerBashTool(api, makeMockContext(bridge));
    const bashTool = tools.get("bash")!;
    await bashTool.execute(
      "host-call",
      { command, workdir: cwd, sandbox: "host" },
      undefined,
      undefined,
      extCtx,
    );

    expect(confirmations).toEqual([
      {
        title: "Run command unsandboxed on host?",
        message: `This command will run UNSANDBOXED on the host.\n\nExact command:\n${command}\n\nWorking directory:\n${cwd}`,
      },
    ]);
    expect(calls).toHaveLength(2);
    expect(calls[1]).toMatchObject({
      command,
      workdir: cwd,
      sandbox: "host",
      permissions_granted: ["esc_server_minted"],
    });
  });

  test("permission_required error throws clear message", async () => {
    const tools = new Map<string, MockToolDef>();
    const api = makeMockApi(tools);

    const mockBridge = {
      send: async () => {
        throw new Error("permission_required: bash command requires permission");
      },
    } as unknown as BinaryBridge;

    const ctx = makeMockContext(mockBridge);
    registerBashTool(api, ctx);

    const bashTool = tools.get("bash")!;
    const extCtx: MockExtensionContext = { cwd: projectRoot, hasUI: false };

    await expect(
      bashTool.execute("test-call", { command: "rm -rf /" }, undefined, undefined, extCtx),
    ).rejects.toThrow("Permission ask reached Pi adapter");
  });

  test("default sync cap rejects 120001 with the config knob in the error", async () => {
    const tools = new Map<string, MockToolDef>();
    const api = makeMockApi(tools);
    const { bridge } = makeTrackableMockBridge({ status: "completed", exit_code: 0 });
    registerBashTool(api, makeMockContext(bridge));

    await expect(
      tools
        .get("bash_watch")!
        .execute(
          "call",
          { task_id: "bash-cap-default", timeout_ms: 120_001 },
          undefined,
          undefined,
          { cwd: projectRoot },
        ),
    ).rejects.toThrow("timeoutMs must be between 1 and 120000 (bash.watch_sync_max_ms)");
  });

  test("configured 30-minute sync cap accepts 1800000", async () => {
    const tools = new Map<string, MockToolDef>();
    const api = makeMockApi(tools);
    const { bridge } = makeTrackableMockBridge({ status: "completed", exit_code: 0 });
    registerBashTool(
      api,
      makeMockContext(bridge, {
        bash: { watch_sync_max_ms: 1_800_000 },
      } as PluginContext["config"]),
    );

    await expect(
      tools
        .get("bash_watch")!
        .execute("call", { task_id: "bash-cap-old", timeout_ms: 1_800_000 }, undefined, undefined, {
          cwd: projectRoot,
        }),
    ).resolves.toMatchObject({
      content: expect.arrayContaining([
        expect.objectContaining({ text: expect.stringContaining("task exited") }),
      ]),
    });
  });

  test("project-resolved sync cap is used at runtime", async () => {
    const tools = new Map<string, MockToolDef>();
    const api = makeMockApi(tools);
    const { bridge, calls } = makeTrackableMockBridge({ status: "completed", exit_code: 0 });
    registerBashTool(
      api,
      makeMockContext(bridge, { bash: { watch_sync_max_ms: 1_000 } } as PluginContext["config"]),
    );

    await expect(
      tools
        .get("bash_watch")!
        .execute("call", { task_id: "bash-cap-project", timeout_ms: 1_001 }, undefined, undefined, {
          cwd: projectRoot,
        }),
    ).rejects.toThrow("timeoutMs must be between 1 and 1000 (bash.watch_sync_max_ms)");
    expect(calls).toHaveLength(0);
  });

  test("bash-family control RPCs keep the bridge on transport timeout", async () => {
    const tools = new Map<string, MockToolDef>();
    const api = makeMockApi(tools);
    const calls: unknown[] = [];
    const bridge = {
      send: async (...args: unknown[]) => {
        calls.push(args);
        const command = args[0];
        if (command === "bash_notify") return { success: true, watch_id: "watch-1" };
        if (command === "bash_write") return { success: true, bytes_written: 3 };
        if (command === "bash_kill") return { success: true, status: "killed" };
        return { success: true, status: "running", duration_ms: 0 };
      },
    } as unknown as BinaryBridge;
    registerBashTool(api, makeMockContext(bridge));
    const extCtx = { cwd: projectRoot };

    await tools
      .get("bash_status")!
      .execute("call", { task_id: "bash-control" }, undefined, undefined, extCtx);
    await tools
      .get("bash_watch")!
      .execute(
        "call",
        { task_id: "bash-control", pattern: "ready", background: true },
        undefined,
        undefined,
        extCtx,
      );
    await tools
      .get("bash_write")!
      .execute("call", { task_id: "bash-control", input: "abc" }, undefined, undefined, extCtx);
    await tools
      .get("bash_kill")!
      .execute("call", { task_id: "bash-control" }, undefined, undefined, extCtx);

    expect(calls.map((call) => (call as [string])[0])).toEqual([
      "bash_status",
      "bash_notify",
      "bash_write",
      "bash_kill",
    ]);
    for (const call of calls as Array<[string, Record<string, unknown>, Record<string, unknown>]>) {
      expect(call[2].keepBridgeOnTimeout).toBe(true);
      expect(call[2].transportTimeoutMs).toBe(30_000);
    }
  });

  test("bash_watch pattern substring returns waited matched details", async () => {
    const outputPath = await spill("alpha ready beta\n");
    try {
      const tools = new Map<string, MockToolDef>();
      const api = makeMockApi(tools);
      const { bridge, calls } = makeTrackableMockBridge({
        status: "running",
        mode: "pipes",
        output_path: outputPath,
      });
      const ctx = makeMockContext(bridge);
      registerBashTool(api, ctx);
      const result = (await tools
        .get("bash_watch")!
        .execute("call", { task_id: "bash-pi-wait", pattern: "ready" }, undefined, undefined, {
          cwd: projectRoot,
        })) as { details: { waited?: { reason: string; match?: string; match_offset?: number } } };
      expect(toolText(result)).toContain('matched "ready" in stdout at offset 6');
      expect(result.details.waited).toMatchObject({
        reason: "matched",
        match: "ready",
        match_offset: 6,
        match_stream: "stdout",
      });
      expect(calls.some((call) => (call as [string])[0] === "bash_regex_match")).toBe(false);
      const callArgs = calls[0] as [string, Record<string, unknown>, Record<string, unknown>];
      expect(callArgs[2].keepBridgeOnTimeout).toBe(true);
      expect(callArgs[2].transportTimeoutMs).toBe(30_000);
    } finally {
      await rm(join(outputPath, ".."), { recursive: true, force: true });
    }
  });

  test("bash_watch pattern regex routes to bridge and returns waited matched details", async () => {
    const outputPath = await spill("abc ready: 4242\n");
    try {
      const tools = new Map<string, MockToolDef>();
      const api = makeMockApi(tools);
      const calls: unknown[] = [];
      const bridge = {
        send: async (...args: unknown[]) => {
          calls.push(args);
          const [command, params] = args as [string, Record<string, unknown>];
          if (command === "bash_regex_match") {
            return {
              success: true,
              matched: params.text === "abc ready: 4242\n",
              match_text: "ready: 4242",
              match_offset: 4,
              match_index_chars: 4,
            };
          }
          return addArtifactBytes(command, params, {
            success: true,
            status: "running",
            mode: "pipes",
            output_path: outputPath,
          });
        },
      } as unknown as BinaryBridge;
      registerBashTool(api, makeMockContext(bridge));

      const result = (await tools
        .get("bash_watch")!
        .execute(
          "call",
          { task_id: "bash-pi-regex", pattern: { regex: "ready: \\d+" } },
          undefined,
          undefined,
          { cwd: projectRoot },
        )) as { details: { waited?: { reason: string; match?: string; match_offset?: number } } };

      expect(toolText(result)).toContain('matched "ready: 4242" in stdout at offset 4');
      expect(result.details.waited).toMatchObject({
        reason: "matched",
        match: "ready: 4242",
        match_offset: 4,
      });
      expect(calls.filter((call) => (call as [string])[0] === "bash_regex_match")).toEqual([
        expect.arrayContaining([
          "bash_regex_match",
          expect.objectContaining({ pattern: "ready: \\d+", text: "" }),
        ]),
        expect.arrayContaining([
          "bash_regex_match",
          expect.objectContaining({ pattern: "ready: \\d+", text: "abc ready: 4242\n" }),
        ]),
      ]);
    } finally {
      await rm(join(outputPath, ".."), { recursive: true, force: true });
    }
  });

  test("bash_watch pattern regex surfaces invalid_regex as invalid_request", async () => {
    const outputPath = await spill("abc ready\n");
    try {
      const tools = new Map<string, MockToolDef>();
      const api = makeMockApi(tools);
      const bridge = {
        send: async (command: string) => {
          if (command === "bash_regex_match") {
            return { success: false, code: "invalid_regex", message: "unclosed group" };
          }
          return {
            success: true,
            status: "running",
            mode: "pipes",
            output_path: outputPath,
          };
        },
      } as unknown as BinaryBridge;
      registerBashTool(api, makeMockContext(bridge));

      await expect(
        tools
          .get("bash_watch")!
          .execute(
            "call",
            { task_id: "bash-pi-regex-invalid", pattern: { regex: "(" } },
            undefined,
            undefined,
            { cwd: projectRoot },
          ),
      ).rejects.toThrow("invalid_request: invalid_regex");
    } finally {
      await rm(join(outputPath, ".."), { recursive: true, force: true });
    }
  });

  test("bash_watch scans stderr_path as well as output_path", async () => {
    const spill = await spillPair("stdout\n", "warning: READY on stderr\n");
    try {
      const tools = new Map<string, MockToolDef>();
      const api = makeMockApi(tools);
      const { bridge } = makeTrackableMockBridge({
        status: "running",
        mode: "pipes",
        output_path: spill.stdoutPath,
        stderr_path: spill.stderrPath,
      });
      const ctx = makeMockContext(bridge);
      registerBashTool(api, ctx);
      const result = (await tools
        .get("bash_watch")!
        .execute("call", { task_id: "bash-pi-stderr", pattern: "READY" }, undefined, undefined, {
          cwd: projectRoot,
        })) as { details: { waited?: { reason: string; match?: string; match_offset?: number } } };
      expect(toolText(result)).toContain('matched "READY" in stderr at offset 9');
      expect(result.details.waited).toMatchObject({
        reason: "matched",
        match: "READY",
        match_offset: 9,
        match_stream: "stderr",
      });
    } finally {
      await rm(spill.dir, { recursive: true, force: true });
    }
  });

  test("bash_watch does not combine stdout and stderr into a fabricated match", async () => {
    const spill = await spillPair("ERR", "OR");
    try {
      const tools = new Map<string, MockToolDef>();
      const api = makeMockApi(tools);
      const { bridge } = makeTrackableMockBridge({
        status: "completed",
        exit_code: 0,
        mode: "pipes",
        output_path: spill.stdoutPath,
        stderr_path: spill.stderrPath,
      });
      registerBashTool(api, makeMockContext(bridge));

      const result = (await tools
        .get("bash_watch")!
        .execute(
          "call",
          { task_id: "bash-pi-stream-boundary", pattern: "ERROR" },
          undefined,
          undefined,
          { cwd: projectRoot },
        )) as { details: { waited?: { reason: string } } };

      expect(toolText(result)).toContain("task exited (completed, exit 0)");
      expect(toolText(result)).not.toContain("matched");
      expect(result.details.waited?.reason).toBe("exited");
    } finally {
      await rm(spill.dir, { recursive: true, force: true });
    }
  });

  test("bash_watch preserves a same-stream match split across poll reads", async () => {
    const spill = await spillPair("RE", "");
    let polls = 0;
    try {
      const tools = new Map<string, MockToolDef>();
      const api = makeMockApi(tools);
      const bridge = {
        send: async (command: string, params: Record<string, unknown> = {}) => {
          if (command === "bash_status") {
            polls += 1;
            if (polls === 2) await appendFile(spill.stdoutPath, "ADY");
          }
          return addArtifactBytes(command, params, {
            success: true,
            status: "running",
            mode: "pipes",
            output_path: spill.stdoutPath,
            stderr_path: spill.stderrPath,
          });
        },
      } as unknown as BinaryBridge;
      registerBashTool(api, makeMockContext(bridge));

      const result = (await tools
        .get("bash_watch")!
        .execute(
          "call",
          { task_id: "bash-pi-same-stream-boundary", pattern: "READY", timeout_ms: 500 },
          undefined,
          undefined,
          { cwd: projectRoot },
        )) as {
        details: {
          waited?: {
            reason: string;
            match?: string;
            match_offset?: number;
            match_stream?: string;
          };
        };
      };

      expect(toolText(result)).toContain('matched "READY" in stdout at offset 0');
      expect(result.details.waited).toMatchObject({
        reason: "matched",
        match: "READY",
        match_offset: 0,
        match_stream: "stdout",
      });
      expect(polls).toBe(2);
    } finally {
      await rm(spill.dir, { recursive: true, force: true });
    }
  });

  test("bash_watch scans terminal output before returning exited", async () => {
    const outputPath = await spill("pattern exists and match wins\n");
    try {
      const tools = new Map<string, MockToolDef>();
      const api = makeMockApi(tools);
      const { bridge } = makeTrackableMockBridge({
        status: "completed",
        exit_code: 0,
        mode: "pipes",
        output_path: outputPath,
      });
      const ctx = makeMockContext(bridge);
      registerBashTool(api, ctx);
      const result = (await tools
        .get("bash_watch")!
        .execute("call", { task_id: "bash-pi-race", pattern: "pattern" }, undefined, undefined, {
          cwd: projectRoot,
        })) as { details: { waited?: { reason: string; match?: string; match_offset?: number } } };
      expect(toolText(result)).toContain('matched "pattern" in stdout at offset 0');
      expect(toolText(result)).not.toContain("task exited");
      expect(result.details.waited).toMatchObject({ reason: "matched", match_offset: 0 });
    } finally {
      await rm(join(outputPath, ".."), { recursive: true, force: true });
    }
  });

  test("bash_watch exit-only returns waited exited details", async () => {
    const tools = new Map<string, MockToolDef>();
    const api = makeMockApi(tools);
    const { bridge } = makeTrackableMockBridge({
      status: "completed",
      exit_code: 0,
      output_preview: "done",
    });
    const ctx = makeMockContext(bridge);
    registerBashTool(api, ctx);
    // bash_watch with no pattern in sync mode waits for exit
    const result = (await tools
      .get("bash_watch")!
      .execute("call", { task_id: "bash-pi-exit" }, undefined, undefined, {
        cwd: projectRoot,
      })) as { details: { waited?: { reason: string } } };
    expect(toolText(result)).toContain("task exited (completed, exit 0)");
    expect(result.details.waited?.reason).toBe("exited");
  });

  test("renderCall returns Text component", () => {
    const tools = new Map<string, MockToolDef>();
    const api = makeMockApi(tools);
    const mockBridge = makeMockBridge();
    const ctx = makeMockContext(mockBridge);

    registerBashTool(api, ctx);

    const bashTool = tools.get("bash")!;
    expect(bashTool.renderCall).toBeDefined();

    // With description
    const withDesc = bashTool.renderCall!(
      { command: "echo test", description: "Print greeting" },
      mockTheme,
      { lastComponent: undefined, isError: false },
    );
    expect(withDesc).toBeInstanceOf(Text);

    // With long command (should be shortened)
    const longCmd = "a".repeat(100);
    const withLongCmd = bashTool.renderCall!({ command: longCmd }, mockTheme, {
      lastComponent: undefined,
      isError: false,
    });
    expect(withLongCmd).toBeInstanceOf(Text);
  });

  test("renderResult returns appropriate component types", () => {
    const tools = new Map<string, MockToolDef>();
    const api = makeMockApi(tools);
    const mockBridge = makeMockBridge();
    const ctx = makeMockContext(mockBridge);

    registerBashTool(api, ctx);

    const bashTool = tools.get("bash")!;
    expect(bashTool.renderResult).toBeDefined();

    // Success result with bg_completions
    const successResult = {
      content: [{ type: "text", text: "output" }],
      details: {
        exit_code: 0,
        duration_ms: 150,
        bg_completions: [
          { task_id: "task-1", status: "completed", exit_code: 0, command: "npm install" },
        ],
      },
    };

    const rendered = bashTool.renderResult!(successResult, { expanded: true }, mockTheme, {
      lastComponent: undefined,
      isError: false,
    });

    expect(rendered).toBeInstanceOf(Container);

    // Error result
    const errorResult = {
      content: [{ type: "text", text: "Command failed" }],
      details: { exit_code: 1 },
    };

    const errorRendered = bashTool.renderResult!(errorResult, {}, mockTheme, {
      lastComponent: undefined,
      isError: true,
    });

    expect(errorRendered).toBeInstanceOf(Text);
  });

  test("renderResult follows Pi's collapsed and expanded result contract", () => {
    const tools = new Map<string, MockToolDef>();
    registerBashTool(makeMockApi(tools), makeMockContext(makeMockBridge()));
    const bashTool = tools.get("bash")!;
    const largeResult = {
      content: [
        {
          type: "text",
          text: ["first output", "second", "third", "fourth", "fifth", "sixth"].join("\n"),
        },
      ],
      details: { exit_code: 0 },
    };
    const renderText = (value: unknown): string =>
      (value as { render: (width: number) => string[] }).render(200).join("\n");

    const collapsed = renderText(
      bashTool.renderResult!(largeResult, { expanded: false }, mockTheme, {
        lastComponent: undefined,
        isError: false,
      }),
    );
    expect(collapsed).toContain("exit 0: first output");
    expect(collapsed).not.toContain("sixth");

    const omitted = renderText(
      bashTool.renderResult!(largeResult, undefined, mockTheme, {
        lastComponent: undefined,
        isError: false,
      }),
    );
    expect(omitted).toContain("exit 0: first output");
    expect(omitted).not.toContain("sixth");

    const expanded = renderText(
      bashTool.renderResult!(largeResult, { expanded: true }, mockTheme, {
        lastComponent: undefined,
        isError: false,
      }),
    );
    expect(expanded).toContain("sixth");

    const tiny = renderText(
      bashTool.renderResult!(
        { content: [{ type: "text", text: "one\ntwo" }], details: { exit_code: 0 } },
        { expanded: false },
        mockTheme,
        { lastComponent: undefined, isError: false },
      ),
    );
    expect(tiny).toContain("one");
    expect(tiny).toContain("two");
  });

  test("handles missing bg_completions gracefully", async () => {
    const tools = new Map<string, MockToolDef>();
    const api = makeMockApi(tools);
    const mockBridge = makeMockBridge({
      output: "Simple output",
      exit_code: 0,
      // No bg_completions field
    });
    const ctx = makeMockContext(mockBridge);

    registerBashTool(api, ctx);

    const bashTool = tools.get("bash")!;
    const extCtx: MockExtensionContext = { cwd: projectRoot, hasUI: false };

    const result = (await bashTool.execute(
      "test-call",
      { command: "echo test" },
      undefined,
      undefined,
      extCtx,
    )) as { content: Array<{ text: string }>; details: { bg_completions?: unknown[] } };

    // Should not have bg_completions in details
    expect(result.details.bg_completions).toBeUndefined();

    // Text should not contain background task notifications
    expect(result.content[0].text).toBe("Simple output");
  });
});

/**
 * Verify that the primary `bash` tool can be registered without exposing the
 * background control surface. `registerBashTool` is called only after Pi's
 * outer bash-enabled gate passes; inside it, `bash.background` controls
 * `bash_status` / `bash_kill` / `bash_write` / `bash_watch` registration.
 */
describe("registerBashTool gates background control tools", () => {
  function registerWithConfig(config: PluginContext["config"]) {
    const tools = new Map<string, MockToolDef>();
    const api = makeMockApi(tools);
    const bridge = makeMockBridge();
    const ctx: PluginContext = {
      pool: { getBridge: () => bridge } as unknown as PluginContext["pool"],
      config,
      storageDir: "/tmp/test",
    };
    registerBashTool(api, ctx);
    return tools;
  }

  function expectBackgroundControls(tools: Map<string, MockToolDef>, present: boolean): void {
    for (const name of ["bash_status", "bash_watch", "bash_write", "bash_kill"]) {
      if (present) {
        expect(tools.get(name)).toBeDefined();
      } else {
        expect(tools.get(name)).toBeUndefined();
      }
    }
  }

  test("default resolved config → full bash surface registered", () => {
    const tools = registerWithConfig({} as PluginContext["config"]);
    expect(tools.get("bash")).toBeDefined();
    expectBackgroundControls(tools, true);
  });

  test("bash.background=false → bash registered without background controls", () => {
    const tools = registerWithConfig({ bash: { background: false } } as PluginContext["config"]);
    expect(tools.get("bash")).toBeDefined();
    expectBackgroundControls(tools, false);
  });

  test("legacy rewrite=true only → bash registered without background controls", () => {
    const tools = registerWithConfig({
      experimental: { bash: { rewrite: true } },
    } as PluginContext["config"]);
    expect(tools.get("bash")).toBeDefined();
    expectBackgroundControls(tools, false);
  });

  test("legacy compress=true only → bash registered without background controls", () => {
    const tools = registerWithConfig({
      experimental: { bash: { compress: true } },
    } as PluginContext["config"]);
    expect(tools.get("bash")).toBeDefined();
    expectBackgroundControls(tools, false);
  });

  test("legacy background=true → full bash surface registered", () => {
    const tools = registerWithConfig({
      experimental: { bash: { background: true } },
    } as PluginContext["config"]);
    expect(tools.get("bash")).toBeDefined();
    expectBackgroundControls(tools, true);
  });

  test("all legacy flags true → full bash surface registered", () => {
    const tools = registerWithConfig({
      experimental: { bash: { rewrite: true, compress: true, background: true } },
    } as PluginContext["config"]);
    expect(tools.get("bash")).toBeDefined();
    expectBackgroundControls(tools, true);
  });

  test("bash_status and bash_kill execute with task_id request shape", async () => {
    const tools = new Map<string, MockToolDef>();
    const api = makeMockApi(tools);
    const { bridge, calls } = makeTrackableMockBridge({ status: "completed", exit_code: 0 });
    const ctx: PluginContext = {
      pool: { getBridge: () => bridge } as unknown as PluginContext["pool"],
      config: { experimental: { bash: { background: true } } } as PluginContext["config"],
      storageDir: "/tmp/test",
    };
    registerBashTool(api, ctx);

    await tools
      .get("bash_status")!
      .execute("status-call", { task_id: "bash-123" }, undefined, undefined, {
        cwd: projectRoot,
      });
    await tools
      .get("bash_kill")!
      .execute("kill-call", { task_id: "bash-123" }, undefined, undefined, {
        cwd: projectRoot,
      });

    expect((calls[0] as [string, Record<string, unknown>])[0]).toBe("bash_status");
    expect((calls[0] as [string, Record<string, unknown>])[1]).toEqual({ task_id: "bash-123" });
    expect((calls[1] as [string, Record<string, unknown>])[0]).toBe("bash_kill");
    expect((calls[1] as [string, Record<string, unknown>])[1]).toEqual({ task_id: "bash-123" });
  });
});

describe("bash tool description (agent-facing wording)", () => {
  function registeredDescription(
    aftSearchRegistered: boolean,
    configOverride?: Record<string, unknown>,
  ): {
    description: string;
    promptGuidelines: string[];
  } {
    let captured: { description: string; promptGuidelines: string[] } | null = null;
    const api = {
      registerTool: (def: { name: string; description: string; promptGuidelines: string[] }) => {
        // registerBashTool registers bash + bash_status/kill/watch/write —
        // only the bash tool itself carries the code-search prohibition.
        if (def.name !== "bash") return;
        captured = { description: def.description, promptGuidelines: def.promptGuidelines };
      },
    } as unknown as ExtensionAPI;
    const ctx = makeMockContext({} as BinaryBridge);
    if (configOverride) ctx.config = configOverride as PluginContext["config"];
    registerBashTool(api, ctx, aftSearchRegistered);
    if (!captured) throw new Error("registerTool was not called");
    return captured;
  }

  test("prohibits bash code search and steers to aft_search when registered", () => {
    const { description, promptGuidelines } = registeredDescription(true);
    expect(description).toContain("DO NOT use bash for code search");
    expect(description).toContain("STOP");
    expect(description).toContain("aft_search");
    expect(promptGuidelines.join("\n")).toContain("DO NOT use bash for code search");
  });

  test("replaces zoom steering with read when zoom is disabled", () => {
    const { description } = registeredDescription(true, { disabled_tools: ["aft_zoom"] });
    expect(description).toContain("aft_search");
    expect(description).toContain("aft_outline");
    expect(description).toContain("read");
    expect(description).not.toContain("aft_zoom");
  });

  test("steers to the grep tool when aft_search is not registered", () => {
    const { description } = registeredDescription(false);
    expect(description).toContain("DO NOT use bash for code search");
    expect(description).toContain("`grep` tool");
    expect(description).not.toContain("aft_search");
  });

  test("contains no internal vocabulary agents don't care about", () => {
    for (const flag of [true, false]) {
      const { description } = registeredDescription(flag);
      expect(description.toLowerCase()).not.toContain("hoisted");
      expect(description.toLowerCase()).not.toContain("rust bash handler");
      expect(description.toLowerCase()).not.toContain("rewrit");
    }
  });

  test("background on is foreground-first and names the background+bash_watch anti-pattern", () => {
    const on = registeredDescription(true).description;
    // Lead with foreground-returns-inline and the explicit wait knob for known long commands.
    expect(on).toContain("foreground");
    expect(on).toContain("wait: true");
    expect(on).toContain("auto-promote can remind you while you work");
    // Demote background: true to the parallel-work-only case.
    expect(on).toContain("other useful work to do while it runs");
    // Name the exact anti-pattern this description previously caused.
    expect(on).toContain("never background a command and immediately `bash_watch` it");
    // Still forbid the bash_status polling loop.
    expect(on).toContain("never loop `bash_status` to wait");
  });

  test("bash_watch and bash_status agent-facing descriptions", () => {
    const ctx = makeMockContext({} as BinaryBridge);
    const watch = createBashWatchTool(ctx);
    expect(watch.description).toContain("short remaining wait on a task");
    expect(watch.description).toContain("120s by default");
    expect(watch.description).toContain("bash({background:true})");
    expect(watch.description).toContain("Never loop bash_status");
    const status = createBashStatusTool(ctx);
    expect(status.description).toContain("never loop");
    expect(status.description).toContain("To wait, use bash_watch");
  });

  test("bash background param steers to bash_watch not poll", () => {
    let backgroundDesc = "";
    const api = {
      registerTool: (def: {
        name: string;
        parameters?: { properties?: Record<string, unknown> };
      }) => {
        if (def.name !== "bash") return;
        const bg = def.parameters?.properties?.background as { description?: string } | undefined;
        backgroundDesc = bg?.description ?? "";
      },
    } as unknown as ExtensionAPI;
    registerBashTool(api, makeMockContext({} as BinaryBridge), false);
    expect(backgroundDesc).toContain("bash_watch");
    expect(backgroundDesc).not.toContain("poll completion");
  });

  test("compression and background sentences track the resolved bash config", () => {
    // Default mock config resolves to compress+background on.
    const on = registeredDescription(true).description;
    expect(on).toContain("compressed: false");
    expect(on).toContain("background: true");
    expect(on).toContain("pty: true");
    expect(on).toContain("bash_watch");

    // Both features off: foreground commands block to completion, so do not
    // advertise promotion or any background-control tools.
    const off = registeredDescription(true, {
      bash: { compress: false, background: false },
    }).description;
    expect(off).toContain("foreground to completion");
    expect(off).not.toContain("compressed: false");
    expect(off).not.toContain("background: true");
    expect(off).not.toContain("pty: true");
    expect(off).not.toContain("promoted");
    expect(off).not.toContain("bash_status");
    expect(off).not.toContain("bash_kill");
    expect(off).not.toContain("bash_watch");
  });
});
