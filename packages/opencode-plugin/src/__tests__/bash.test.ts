/// <reference path="../bun-test.d.ts" />
import { describe, expect, mock, test } from "bun:test";
import { appendFile, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import {
  BASH_HOST_FALLBACK_BANNER,
  type BridgePool,
  type BridgeRequestOptions,
  BridgeTransportUnavailableError,
} from "@cortexkit/aft-bridge";
import { type ToolContext, tool } from "@opencode-ai/plugin";
import { withEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import {
  __resetBgNotificationStateForTests,
  sessionBgStates,
  trackBgTask,
} from "../bg-notifications.js";
import { _resetSubagentCacheForTest } from "../shared/subagent-detect.js";
import { __resetSyncWatchAbortForTests, signalSyncWatchAbort } from "../sync-watch-abort.js";
import {
  bashToolDescription,
  createBashKillTool,
  createBashStatusTool,
  createBashTool,
} from "../tools/bash.js";
import { createBashWatchTool } from "../tools/bash_watch.js";
import { createBashWriteTool } from "../tools/bash_write.js";
import type { PluginContext } from "../types.js";
import { mockAsk, noopAsk } from "./test-helpers";

const PROJECT_CWD = resolve(import.meta.dir, "../../../..");

/**
 * The hoisted `bash` tool now returns `{ output, title, metadata }` (UI
 * metadata lives on the result, not a side-channel store). Most tests only
 * care about the agent-visible text — this unwraps it (and tolerates the
 * legacy bare-string shape for safety).
 */
function bashText(r: unknown): string {
  return typeof r === "string" ? r : ((r as { output?: string })?.output ?? "");
}

type BridgeResponse = Record<string, unknown>;
type SendCall = {
  command: string;
  params: Record<string, unknown>;
  options?: BridgeRequestOptions;
};
type ProgressHandler = (frame: { text: string }) => void;

async function addArtifactBytes(
  command: string,
  params: Record<string, unknown>,
  response: BridgeResponse,
): Promise<BridgeResponse> {
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
  aftSearchRegistered = false,
  config: PluginContext["config"] = {} as PluginContext["config"],
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
    config,
    storageDir: "/tmp/aft-test",
  };
  return { calls, tool: createBashTool(ctx, aftSearchRegistered) };
}

function safeParse(schema: unknown, value: unknown): { success: boolean } {
  return (schema as SafeParseSchema).safeParse(value);
}

describe("OpenCode bash adapter", () => {
  test("schema accepts valid unified bash params and rejects invalid shapes", () => {
    const { tool: bash } = createHarness(() => ({ success: true, output: "" }));

    expect(bash.description).toContain("Output is compressed by default");
    expect(bash.description).toContain("compressed: false");
    expect(bash.description).toContain("Piped commands run verbatim");
    expect(bash.description).toContain("background: true");
    expect(bash.description).toContain("wait: true");

    expect(safeParse(bash.args.command, "ls -la").success).toBe(true);
    expect(safeParse(bash.args.timeout, 120_000).success).toBe(true);
    expect(safeParse(bash.args.workdir, PROJECT_CWD).success).toBe(true);
    expect(safeParse(bash.args.description, "List files").success).toBe(true);
    expect(safeParse(bash.args.wait, true).success).toBe(true);
    expect(safeParse(bash.args.sandbox, "host").success).toBe(true);
    expect(safeParse(bash.args.sandbox, "native").success).toBe(false);
    expect(safeParse(bash.args.background, true).success).toBe(true);
    expect(safeParse(bash.args.compressed, false).success).toBe(true);
    expect(safeParse(bash.args.ptyRows, 50).success).toBe(true);
    expect(safeParse(bash.args.ptyCols, 120).success).toBe(true);

    expect(safeParse(bash.args.command, 123).success).toBe(false);
    expect(safeParse(bash.args.timeout, "slow").success).toBe(false);
    expect(safeParse(bash.args.wait, "true").success).toBe(false);
    expect(safeParse(bash.args.background, "yes").success).toBe(false);
    expect(safeParse(bash.args.compressed, "no").success).toBe(false);
    // optionalInt is a plain bounded `z.number().int().min(...).max(...).optional()`
    // schema (deliberately NOT a transform; transforms break OpenCode's
    // z.toJSONSchema and crash plugin load — see
    // `tool-schemas-json-convertible.test.ts`). Out-of-range and non-integer
    // values are rejected at Zod parse time.
    expect(safeParse(bash.args.ptyRows, 0).success).toBe(false);
    expect(safeParse(bash.args.ptyRows, 61).success).toBe(false);
    expect(safeParse(bash.args.ptyRows, 1.5).success).toBe(false);
    expect(safeParse(bash.args.ptyCols, 141).success).toBe(false);

    // Verify the args convert to JSON Schema with the default options
    // OpenCode uses (`{ io: "input" }`, no `unrepresentable: "any"` escape
    // hatch). If any arg's schema contains a transform, this throws and
    // plugin load fails at session start.
    for (const schema of Object.values(bash.args)) {
      expect(() => tool.schema.toJSONSchema(schema, { io: "input" })).not.toThrow();
      const jsonSchema = tool.schema.toJSONSchema(schema, { io: "input" }) as {
        description?: string;
      };
      expect(jsonSchema.description?.length).toBeGreaterThan(20);
    }
  });

  test("schema omits background and PTY args when bash.background is disabled", () => {
    const { tool: bash } = createHarness(() => ({ success: true, output: "" }), undefined, false, {
      bash: { background: false },
    } as PluginContext["config"]);

    expect(Object.keys(bash.args)).toEqual([
      "command",
      "timeout",
      "workdir",
      "description",
      "wait",
      "sandbox",
      "compressed",
    ]);
    expect(bash.args.background).toBeUndefined();
    expect(bash.args.pty).toBeUndefined();
    expect(bash.args.ptyRows).toBeUndefined();
    expect(bash.args.ptyCols).toBeUndefined();
    expect(bash.description).toContain("foreground to completion");
    expect(bash.description).not.toContain("background: true");
    expect(bash.description).not.toContain("bash_status");
    expect(bash.description).not.toContain("bash_kill");
    expect(bash.description).not.toContain("bash_watch");
    expect(bash.description).not.toContain("pty: true");

    const timeoutSchema = tool.schema.toJSONSchema(bash.args.timeout, { io: "input" }) as {
      description?: string;
    };
    expect(timeoutSchema.description).toContain("returns inline");
    expect(timeoutSchema.description).not.toContain("promoted");
  });

  test("pty dimensions are forwarded when pty:true and silently ignored when pty:false", async () => {
    const { calls, tool: bash } = createHarness((_command, params) => ({
      success: true,
      status: "running",
      task_id: "bash-pty-dims",
      output: params.pty
        ? 'PTY task started: bash-pty-dims. Use bash_status({ taskId: "bash-pty-dims", outputMode: "screen" }) to see the visible terminal, bash_write({ taskId: "bash-pty-dims", input: ... }) to send keystrokes. A completion reminder fires automatically when the task exits.'
        : "Background task started: bash-pty-dims. A completion reminder will be delivered automatically; don't poll bash_status.",
    }));

    // pty:false + ptyRows passed defensively: should NOT throw, dims silently ignored
    const nonPtyOutput = bashText(
      await bash.execute(
        { command: "echo hi", background: true, ptyRows: 50 },
        createMockSdkContext(),
      ),
    );
    expect(nonPtyOutput).toContain("bash-pty-dims");
    // The non-pty call still forwards ptyRows in params (Rust silently ignores
    // when pty:false). We only assert no throw + task_id propagation here.

    const output = bashText(
      await bash.execute(
        { command: "top", background: true, pty: true, ptyRows: 50, ptyCols: 120 },
        createMockSdkContext(),
      ),
    );

    expect(output).toContain("bash-pty-dims");
    expect(calls.at(-1)?.params).toMatchObject({
      pty: true,
      pty_rows: 50,
      pty_cols: 120,
    });
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

    bashText(await bash.execute({ command: "rm -rf /tmp/demo" }, createMockSdkContext({ ask })));

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

  test("host escalation ask shows exact payload and retries with opaque grant", async () => {
    const ask = mockAsk();
    let sendCount = 0;
    const command = "printf 'exact  value'\nprintf done";
    const cwd = "/tmp/exact cwd";
    const { calls, tool: bash } = createHarness(() => {
      sendCount++;
      if (sendCount === 1) {
        return {
          success: false,
          code: "permission_required",
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
      return { success: true, output: "approved", exit_code: 0, truncated: false };
    });

    bashText(
      await bash.execute({ command, workdir: cwd, sandbox: "host" }, createMockSdkContext({ ask })),
    );

    expect(ask).toHaveBeenCalledTimes(1);
    const request = ask.mock.calls[0][0];
    expect(request.permission).toBe("bash");
    expect(request.patterns).toEqual([
      `This command will run UNSANDBOXED on the host.\n\nExact command:\n${command}\n\nWorking directory:\n${cwd}`,
    ]);
    expect(request.metadata).toMatchObject({
      command,
      cwd,
      grant_id: "esc_server_minted",
      unsandboxed: true,
    });
    expect(calls).toHaveLength(2);
    expect(calls[1].params).toMatchObject({
      command,
      workdir: cwd,
      sandbox: "host",
      permissions_granted: ["esc_server_minted"],
    });
  });

  test("gate-off preserves a transport-dead error unchanged", async () => {
    const original = new BridgeTransportUnavailableError("byte-identical transport error");
    const ask = mockAsk();
    const { tool: bash } = createHarness(
      () => {
        throw original;
      },
      undefined,
      false,
      { bash: { host_fallback: false } } as PluginContext["config"],
    );

    let captured: unknown;
    try {
      await bash.execute({ command: "printf should-not-run" }, createMockSdkContext({ ask }));
    } catch (error) {
      captured = error;
    }

    expect(captured).toBe(original);
    expect((captured as Error).message).toBe("byte-identical transport error");
    expect(ask).not.toHaveBeenCalled();
  });

  test("engine-alive errors never engage host fallback", async () => {
    const ask = mockAsk();
    const { tool: bash } = createHarness(
      () => ({ success: false, code: "path_outside_root", message: "outside project root" }),
      undefined,
      false,
      { bash: { host_fallback: true } } as PluginContext["config"],
    );

    await expect(
      bash.execute({ command: "printf should-not-run" }, createMockSdkContext({ ask })),
    ).rejects.toThrow("outside project root");
    expect(ask).not.toHaveBeenCalled();
  });

  // Spawns a real host process through the fallback executor; the first shell
  // exec on a cold CI runner takes seconds, so this budget is explicit rather
  // than Bun's 5s default.
  test("transport-dead fallback recovers on the next successful module call", async () => {
    const ask = mockAsk();
    const command =
      process.platform === "win32"
        ? `${JSON.stringify(process.execPath)} -e "process.stdout.write('opencode-fallback')"`
        : "printf opencode-fallback";
    let attempts = 0;
    const { tool: bash } = createHarness(
      () => {
        attempts += 1;
        if (attempts === 1) throw new BridgeTransportUnavailableError("bridge spawn failed");
        return { success: true, output: "module-recovered", exit_code: 0, truncated: false };
      },
      undefined,
      false,
      { bash: { host_fallback: true } } as PluginContext["config"],
    );

    const output = bashText(await bash.execute({ command }, createMockSdkContext({ ask })));
    const recovered = bashText(
      await bash.execute({ command: "printf module-recovered" }, createMockSdkContext({ ask })),
    );

    expect(output).toStartWith(`${BASH_HOST_FALLBACK_BANNER}\n`);
    expect(output).toContain("opencode-fallback");
    expect(output).toEndWith("[exit code: 0]");
    expect(recovered).toBe("module-recovered");
    expect(recovered).not.toContain(BASH_HOST_FALLBACK_BANNER);
    expect(attempts).toBe(2);
    expect(ask).toHaveBeenCalledTimes(1);
    expect(ask.mock.calls[0][0]).toEqual({
      permission: "bash",
      patterns: [
        `AFT UNAVAILABLE - host fallback execution:\n\nExact command:\n${command}\n\nWorking directory:\n${PROJECT_CWD}`,
      ],
      always: [],
      metadata: { command, cwd: PROJECT_CWD, host_fallback: true },
    });
  }, 20_000);

  test("host fallback refuses background mode instead of spawning locally", async () => {
    const ask = mockAsk();
    const { tool: bash } = createHarness(
      () => {
        throw new BridgeTransportUnavailableError("transport down");
      },
      undefined,
      false,
      { bash: { host_fallback: true } } as PluginContext["config"],
    );

    await expect(
      bash.execute(
        { command: "printf should-not-run", background: true },
        createMockSdkContext({ ask }),
      ),
    ).rejects.toThrow(
      "AFT transport is down; only foreground execution is available in host fallback; background:true is unsupported",
    );
    expect(ask).not.toHaveBeenCalled();
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

    bashText(
      await bash.execute(
        { command: "printenv FOO", workdir: "/tmp/project" },
        createMockSdkContext({ sessionID: "s1", callID: "c1" } as Partial<ToolContext>),
      ),
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

  test("forwards piped commands unchanged and does not append strip notes", async () => {
    const { calls, tool: bash } = createHarness(() => ({
      success: true,
      output: "failure details",
      exit_code: 1,
      truncated: false,
    }));

    const commands = [
      "bun test | grep fail",
      "cargo test | grep -v '^'",
      "cargo test | awk 'END{exit 1}'",
      "pytest -q | grep SENTINEL || exit 1",
    ];

    for (const command of commands) {
      const output = bashText(await bash.execute({ command }, createMockSdkContext()));
      expect(calls.at(-1)?.params.command).toBe(command);
      expect(output).toContain("failure details");
      expect(output).not.toContain("AFT dropped");
    }
  });

  test("keeps filter pipes when compressed:false", async () => {
    const { calls, tool: bash } = createHarness(() => ({
      success: true,
      output: "raw",
      exit_code: 0,
      truncated: false,
    }));

    const output = bashText(
      await bash.execute(
        { command: "bun test | grep fail", compressed: false },
        createMockSdkContext(),
      ),
    );

    expect(calls[0].params.command).toBe("bun test | grep fail");
    expect(output).not.toContain("AFT dropped");
  });

  test('forwards string compressed "false" as boolean false', async () => {
    const { calls, tool: bash } = createHarness(() => ({
      success: true,
      output: "raw",
      exit_code: 0,
      truncated: false,
    }));

    await bash.execute(
      { command: "printf raw", compressed: "false" as unknown as boolean },
      createMockSdkContext(),
    );

    expect(calls[0].params.compressed).toBe(false);
  });

  test("transport timeout is sized to the server-side foreground wait, not user task budget", async () => {
    // The server is responsible for waiting on bash commands and promoting
    // background ones to completion. A user-supplied timeout such as 600_000
    // is sent to the child process as its kill deadline, but the bridge
    // transport timeout only needs to cover the server's foreground wait window
    // plus a small margin for finalization.
    const { calls, tool: bash } = createHarness(() => ({
      success: true,
      output: "built",
      exit_code: 0,
      truncated: false,
    }));

    bashText(
      await bash.execute({ command: "cargo build", timeout: 600_000 }, createMockSdkContext()),
    );

    expect(calls).toHaveLength(1);
    // The user's kill cap still propagates to Rust as the task timeout.
    expect(calls[0].params.timeout).toBe(600_000);
    // The transport timeout is the default foreground wait window (15s) plus a
    // 10s margin for transport finalization.
    expect(calls[0].options?.transportTimeoutMs).toBe(25_000);
    expect(calls[0].options?.keepBridgeOnTimeout).toBe(true);
  });

  test("foreground sub-window timeout is dropped so the bridge applies its default (#102)", async () => {
    // A model passing timeout: 100 must NOT become the bridge kill cap (that
    // killed commands at 100ms). On the foreground path, a timeout below the
    // wait window is incoherent, so it's sent as undefined and Rust uses its
    // 30-minute default.
    const { calls, tool: bash } = createHarness(() => ({
      success: true,
      output: "done",
      exit_code: 0,
      truncated: false,
    }));

    bashText(await bash.execute({ command: "echo hi", timeout: 100 }, createMockSdkContext()));

    expect(calls[0].command).toBe("bash");
    expect(calls[0].params.timeout).toBeUndefined();
  });

  test("explicit background honors a small timeout verbatim as a real kill cap", async () => {
    // Background is the opposite of foreground: a small timeout IS a legitimate
    // kill cap there (kill after N ms), so it must pass through unchanged.
    const { calls, tool: bash } = createHarness(() => ({
      success: true,
      task_id: "task-bg",
      status: "running",
      output:
        "Background task started: task-bg. A completion reminder will be delivered automatically; don't poll bash_status.",
    }));

    bashText(
      await bash.execute(
        { command: "sleep 9", background: true, timeout: 200 },
        createMockSdkContext(),
      ),
    );

    expect(calls[0].command).toBe("bash");
    expect(calls[0].params.timeout).toBe(200);
  });

  test("progress callback forwards rolling output previews through ctx.metadata", async () => {
    const metadata = mock(() => {});
    const { tool: bash } = createHarness((_command, _params, options) => {
      options?.onProgress?.({ text: "hello " });
      options?.onProgress?.({ text: "world" });
      return { success: true, output: "hello world", exit_code: 0, truncated: false };
    });

    bashText(
      await bash.execute(
        { command: "printf hello", description: "Print greeting" },
        createMockSdkContext({ metadata }),
      ),
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

    const output = bashText(
      await bash.execute({ command: "echo foreground" }, createMockSdkContext()),
    );

    expect(output).toBe("foreground");
  });

  test("truncation pointer and exit code are appended to agent-visible output, full payload stored as metadata", async () => {
    const { tool: bash } = createHarness(() => ({
      success: true,
      output: "done\n[output truncated; full output at /tmp/bash-output.txt]",
      exit_code: 0,
      truncated: true,
      output_path: "/tmp/bash-output.txt",
    }));

    const stored = (await bash.execute(
      { command: "echo done", description: "Echo done" },
      createMockSdkContext({
        sessionID: "meta-session",
        callID: "meta-call",
      } as Partial<ToolContext>),
    )) as { output: string; title: string; metadata: Record<string, unknown> };

    // Truncation must be visible to the agent (so it knows full output is on
    // disk); metadata payload preserves the structured fields for the UI.
    expect(stored.output).toBe("done\n[output truncated; full output at /tmp/bash-output.txt]");
    expect(stored.title).toBe("Echo done");
    expect(stored.metadata).toEqual({
      description: "Echo done",
      output: "done\n[output truncated; full output at /tmp/bash-output.txt]",
      exit: 0,
      truncated: true,
      outputPath: "/tmp/bash-output.txt",
    });
  });

  test("non-zero exit code is appended to agent-visible output", async () => {
    const { tool: bash } = createHarness(() => ({
      success: true,
      output: "command failed\n\n[exit code: 2]",
      exit_code: 2,
      truncated: false,
    }));

    const output = bashText(await bash.execute({ command: "false" }, createMockSdkContext()));

    expect(output).toBe("command failed\n\n[exit code: 2]");
  });

  test("background spawn returns a concise started line and stores task metadata", async () => {
    const { tool: bash } = createHarness(() => ({
      success: true,
      status: "running",
      task_id: "task-xyz",
      output:
        "Background task started: task-xyz. A completion reminder will be delivered automatically; don't poll bash_status.",
    }));

    const stored = (await bash.execute(
      { command: "sleep 30 && echo done", background: true },
      createMockSdkContext({
        sessionID: "bg-session",
        callID: "bg-call",
      } as Partial<ToolContext>),
    )) as { output: string; metadata: Record<string, unknown> };

    // The "completion reminder" sentence is load-bearing — it tells the
    // agent the notification mechanism exists so it stops polling. Don't
    // soften this assertion; if the wording changes accidentally we want
    // the test to fail.
    expect(stored.output).toBe(
      "Background task started: task-xyz. A completion reminder will be delivered automatically; don't poll bash_status.",
    );
    expect(stored.metadata).toEqual({
      description: undefined,
      output:
        "Background task started: task-xyz. A completion reminder will be delivered automatically; don't poll bash_status.",
      status: "running",
      taskId: "task-xyz",
    });
  });

  test("already-aborted foreground signal best-effort aborts the in-flight call", async () => {
    const { calls, tool: bash } = createHarness((command) =>
      command === "bash_abort_inflight"
        ? { success: true, killed: 1 }
        : {
            success: true,
            status: "completed",
            task_id: "task-aborted",
            exit_code: 0,
            output: "done",
            truncated: false,
          },
    );
    const controller = new AbortController();
    controller.abort();

    await bash.execute({ command: "sleep 30" }, createMockSdkContext({ abort: controller.signal }));

    expect(calls.map((call) => call.command)).toContain("bash_abort_inflight");
    expect(calls.find((call) => call.command === "bash_abort_inflight")?.params).toMatchObject({
      session_id: "test-session",
    });
  });

  test("normal foreground completion does not fire the abort cleanup call", async () => {
    const { calls, tool: bash } = createHarness(() => ({
      success: true,
      status: "completed",
      task_id: "task-completed",
      exit_code: 0,
      output: "done",
      truncated: false,
    }));
    const controller = new AbortController();

    await bash.execute(
      { command: "echo done" },
      createMockSdkContext({ abort: controller.signal }),
    );
    controller.abort();
    await Promise.resolve();

    expect(calls.map((call) => call.command)).toEqual(["bash"]);
  });

  test("foreground command returns server-orchestrated inline output without client polling", async () => {
    const { calls, tool: bash } = createHarness(() => ({
      success: true,
      status: "completed",
      task_id: "task-inline",
      exit_code: 0,
      duration_ms: 100,
      output: "done",
      truncated: false,
    }));

    const output = bashText(await bash.execute({ command: "printf done" }, createMockSdkContext()));

    expect(output).toBe("done");
    expect(calls.map((call) => call.command)).toEqual(["bash"]);
    expect(calls[0].params).toMatchObject({
      notify_on_completion: false,
      foreground_orchestrate: true,
      block_to_completion: false,
    });
    expect(calls[0].options?.keepBridgeOnTimeout).toBe(true);
    expect(calls[0].options?.transportTimeoutMs).toBe(25_000);
  });

  test("wait true forwards foreground wait mode and scales transport timeout", async () => {
    const { calls, tool: bash } = createHarness(() => ({
      success: true,
      status: "completed",
      task_id: "task-wait",
      exit_code: 0,
      duration_ms: 100,
      output: "waited",
      truncated: false,
    }));

    const output = bashText(
      await bash.execute(
        { command: "sleep 2", wait: "true", timeout: 250 },
        createMockSdkContext(),
      ),
    );

    expect(output).toBe("waited");
    expect(calls.map((call) => call.command)).toEqual(["bash"]);
    expect(calls[0].params).toMatchObject({
      wait: true,
      block_to_completion: true,
      timeout: 250,
      background: false,
      notify_on_completion: false,
    });
    expect(calls[0].options?.transportTimeoutMs).toBe(10_250);
  });

  test("wait true rejects background and pty contradictions", async () => {
    const { tool: bash } = createHarness(() => ({ success: true, output: "" }));

    await expect(
      bash.execute({ command: "sleep 2", wait: true, background: true }, createMockSdkContext()),
    ).rejects.toThrow("wait:true cannot be used with background:true");
    await expect(
      bash.execute({ command: "python", wait: true, pty: true }, createMockSdkContext()),
    ).rejects.toThrow("wait:true cannot be used with pty:true");
  });

  test("foreground leading grep appends aft_search hint", async () => {
    const { tool: bash } = createHarness(
      () => ({
        success: true,
        status: "completed",
        task_id: "task-grep",
        exit_code: 0,
        duration_ms: 100,
        output: "src/file.ts:1:x",
        truncated: false,
      }),
      undefined,
      true,
    );

    const output = bashText(
      await bash.execute({ command: 'grep -nE "x" src/' }, createMockSdkContext()),
    );

    expect(output).toContain("src/file.ts:1:x");
    expect(output).toContain("DO NOT search code by running grep/rg in bash");
    expect(output).toContain("Use the `aft_search` tool instead");
  });

  test("foreground filtering grep does not append code-search hint", async () => {
    const { tool: bash } = createHarness(
      () => ({
        success: true,
        status: "completed",
        task_id: "task-filter",
        exit_code: 0,
        duration_ms: 100,
        output: "failure details",
        truncated: false,
      }),
      undefined,
      true,
    );

    const output = bashText(
      await bash.execute({ command: "bun test | grep fail" }, createMockSdkContext()),
    );

    expect(output).toContain("failure details");
    expect(output).not.toContain("DO NOT search code by running grep/rg in bash");
  });

  test("foreground promotion returns server message without client poll/promote calls", async () => {
    const { calls, tool: bash } = createHarness(() => ({
      success: true,
      status: "running",
      task_id: "task-promote",
      output: `Foreground bash didn't finish within 0s and was promoted to background: task-promote. A completion reminder will be delivered automatically; use bash_status({ taskId: "task-promote" }) to inspect output or bash_kill({ taskId: "task-promote" }) to terminate.`,
    }));

    const output = await withEnv({ AFT_TEST_FOREGROUND_WAIT_MS: "0" }, async () =>
      bashText(
        await bash.execute(
          { command: "sleep 2" },
          createMockSdkContext({ sessionID: "promote-session" }),
        ),
      ),
    );

    expect(output).toContain("promoted to background: task-promote");
    expect(calls.map((call) => call.command)).toEqual(["bash"]);
    expect(calls[0].params.foreground_orchestrate).toBe(true);
    expect(calls[0].params.block_to_completion).toBe(false);
    expect(calls[0].options?.keepBridgeOnTimeout).toBe(true);
    expect(calls[0].options?.transportTimeoutMs).toBe(10_000);
  });

  test("background disabled foreground task is block-to-completion on the server", async () => {
    const { calls, tool: bash } = createHarness(
      () => ({
        success: true,
        status: "completed",
        task_id: "task-no-bg",
        exit_code: 0,
        duration_ms: 125,
        output: "finished without background",
        truncated: false,
      }),
      undefined,
      false,
      { bash: { background: false } } as PluginContext["config"],
    );

    const output = bashText(
      await bash.execute(
        { command: "sleep 2", background: true, pty: true, timeout: 25 },
        createMockSdkContext({ sessionID: "no-bg-session" }),
      ),
    );

    expect(output).toBe("finished without background");
    expect(calls.map((call) => call.command)).toEqual(["bash"]);
    expect(calls.find((call) => call.command === "bash_promote")).toBeUndefined();
    expect(calls[0].params.background).toBe(false);
    expect(calls[0].params.notify_on_completion).toBe(false);
    expect(calls[0].params.pty).toBe(false);
    expect(calls[0].params.timeout).toBe(25);
    expect(calls[0].params.block_to_completion).toBe(true);
    expect(calls[0].options?.transportTimeoutMs).toBe(10_025);
  });

  test("explicit background spawn enables completion notifications", async () => {
    const { calls, tool: bash } = createHarness(() => ({
      success: true,
      status: "running",
      task_id: "task-notify",
      output:
        "Background task started: task-notify. A completion reminder will be delivered automatically; don't poll bash_status.",
    }));

    const output = bashText(
      await bash.execute({ command: "sleep 30", background: true }, createMockSdkContext()),
    );

    expect(output).toContain("Background task started: task-notify");
    expect(calls).toHaveLength(1);
    expect(calls[0].params.notify_on_completion).toBe(true);
  });
});

describe("bash_status tool", () => {
  function makeCtx(
    sendImpl: (
      cmd: string,
      params: Record<string, unknown>,
      options?: BridgeRequestOptions,
    ) => BridgeResponse | Promise<BridgeResponse>,
    config: PluginContext["config"] = {} as PluginContext["config"],
  ) {
    const calls: Array<{
      cmd: string;
      params: Record<string, unknown>;
      options?: BridgeRequestOptions;
    }> = [];
    const bridge = {
      send: async (
        cmd: string,
        params: Record<string, unknown> = {},
        options?: BridgeRequestOptions,
      ) => {
        calls.push({ cmd, params, options });
        return addArtifactBytes(cmd, params, await sendImpl(cmd, params, options));
      },
    };
    const pool = { getBridge: () => bridge } as unknown as BridgePool;
    const ctx: PluginContext = {
      pool,
      client: createMockClient(),
      config,
      storageDir: "/tmp/aft-test",
    };
    return {
      calls,
      ctx,
      statusTool: createBashStatusTool(ctx),
      watchTool: createBashWatchTool(ctx),
      killTool: createBashKillTool(ctx),
      writeTool: createBashWriteTool(ctx),
    };
  }

  test("default sync cap rejects 120001 with the config knob in the error", async () => {
    const { watchTool } = makeCtx(() => ({ success: true, status: "completed", exit_code: 0 }));

    await expect(
      watchTool.execute({ taskId: "bash-cap-default", timeoutMs: 120_001 }, createMockSdkContext()),
    ).rejects.toThrow("timeoutMs must be between 1 and 120000 (bash.watch_sync_max_ms)");
  });

  test("configured 30-minute sync cap accepts 1800000", async () => {
    const { watchTool } = makeCtx(() => ({ success: true, status: "completed", exit_code: 0 }), {
      bash: { watch_sync_max_ms: 1_800_000 },
    } as PluginContext["config"]);

    await expect(
      watchTool.execute({ taskId: "bash-cap-old", timeoutMs: 1_800_000 }, createMockSdkContext()),
    ).resolves.toContain("task exited");
  });

  test("project-resolved sync cap is used at runtime", async () => {
    const { calls, watchTool } = makeCtx(
      () => ({ success: true, status: "completed", exit_code: 0 }),
      { bash: { watch_sync_max_ms: 1_000 } } as PluginContext["config"],
    );

    await expect(
      watchTool.execute({ taskId: "bash-cap-project", timeoutMs: 1_001 }, createMockSdkContext()),
    ).rejects.toThrow("timeoutMs must be between 1 and 1000 (bash.watch_sync_max_ms)");
    expect(calls).toHaveLength(0);
  });

  test("bash-family control RPCs keep the bridge on transport timeout", async () => {
    const { calls, statusTool, watchTool, writeTool, killTool } = makeCtx((cmd) => {
      if (cmd === "bash_notify") return { success: true, watch_id: "watch-1" };
      if (cmd === "bash_write") return { success: true, bytes_written: 3 };
      if (cmd === "bash_kill") return { success: true, status: "killed" };
      return { success: true, status: "running", duration_ms: 0 };
    });
    const runtime = createMockSdkContext();

    await statusTool.execute({ taskId: "bash-control" }, runtime);
    await watchTool.execute(
      { taskId: "bash-control", pattern: "ready", background: true },
      runtime,
    );
    await writeTool.execute({ taskId: "bash-control", input: "abc" }, runtime);
    await killTool.execute({ taskId: "bash-control" }, runtime);

    expect(calls.map((call) => call.cmd)).toEqual([
      "bash_status",
      "bash_notify",
      "bash_write",
      "bash_kill",
    ]);
    for (const call of calls) {
      expect(call.options?.keepBridgeOnTimeout).toBe(true);
    }
    expect(calls.find((call) => call.cmd === "bash_status")?.options?.transportTimeoutMs).toBe(
      30_000,
    );
    expect(calls.find((call) => call.cmd === "bash_notify")?.options?.transportTimeoutMs).toBe(
      30_000,
    );
    expect(calls.find((call) => call.cmd === "bash_kill")?.options?.transportTimeoutMs).toBe(
      30_000,
    );
  });

  test("async bash_watch registration does not add synthetic outstanding task", async () => {
    __resetBgNotificationStateForTests();
    const { watchTool } = makeCtx((cmd) =>
      cmd === "bash_notify"
        ? { success: true, watch_id: "watch-1" }
        : { success: true, status: "completed", exit_code: 0 },
    );

    await watchTool.execute(
      { taskId: "bash-finished", pattern: "READY", background: true },
      createMockSdkContext({ sessionID: "s-watch" }),
    );

    expect(sessionBgStates.get("s-watch")?.outstandingTaskIds.has("bash-finished")).toBe(false);
  });

  test('async bash_watch forwards string once "false" as boolean false', async () => {
    const { calls, watchTool } = makeCtx((cmd) =>
      cmd === "bash_notify"
        ? { success: true, watch_id: "watch-sticky" }
        : { success: true, status: "running" },
    );

    await watchTool.execute(
      {
        taskId: "bash-watch-sticky",
        pattern: "READY",
        background: true,
        once: "false" as unknown as boolean,
      },
      createMockSdkContext(),
    );

    expect(calls.find((call) => call.cmd === "bash_notify")?.params.once).toBe(false);
  });

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
      message:
        "background task not found: bash-unknown. Task IDs only come from a bash tool result or completion notice. If you never received one, the command was not promoted — re-run the command instead of polling.",
    }));
    await expect(
      statusTool.execute({ taskId: "bash-unknown" }, createMockSdkContext()),
    ).rejects.toThrow(
      "background task not found: bash-unknown. Task IDs only come from a bash tool result or completion notice. If you never received one, the command was not promoted — re-run the command instead of polling.",
    );
  });

  async function spill(contents: string): Promise<string> {
    const dir = await mkdtemp(join(tmpdir(), "aft-bash-status-test-"));
    const file = join(dir, "task.out");
    await writeFile(file, contents);
    return file;
  }

  async function spillPair(
    stdout: string,
    stderr: string,
  ): Promise<{ dir: string; stdoutPath: string; stderrPath: string }> {
    const dir = await mkdtemp(join(tmpdir(), "aft-bash-status-test-"));
    const stdoutPath = join(dir, "task.out");
    const stderrPath = join(dir, "task.err");
    await writeFile(stdoutPath, stdout);
    await writeFile(stderrPath, stderr);
    return { dir, stdoutPath, stderrPath };
  }

  test("bash_watch pattern substring match returns matched reason, text, and offset", async () => {
    const outputPath = await spill("prefix Server listening on port 3000\n");
    try {
      const metadata = mock(() => {});
      const { calls, watchTool } = makeCtx(() => ({
        success: true,
        status: "running",
        mode: "pipes",
        output_path: outputPath,
      }));
      const result = await watchTool.execute(
        { taskId: "bash-wait", pattern: "Server listening" },
        createMockSdkContext({ metadata }),
      );
      expect(result).toContain('matched "Server listening" in stdout at offset 7');
      expect(metadata.mock.calls.at(-1)?.[0].waited).toMatchObject({
        reason: "matched",
        match: "Server listening",
        match_offset: 7,
        match_stream: "stdout",
      });
      expect(calls.some((call) => call.cmd === "bash_regex_match")).toBe(false);
    } finally {
      await rm(join(outputPath, ".."), { recursive: true, force: true });
    }
  });

  test("bash_watch pattern regex match routes to bridge and returns matched details", async () => {
    const outputPath = await spill("abc ready: 4242\n");
    try {
      const { calls, watchTool } = makeCtx((cmd, params) => {
        if (cmd === "bash_regex_match") {
          return {
            success: true,
            matched: params.text === "abc ready: 4242\n",
            match_text: "ready: 4242",
            match_offset: 4,
            match_index_chars: 4,
          };
        }
        return {
          success: true,
          status: "running",
          mode: "pipes",
          output_path: outputPath,
        };
      });
      const result = await watchTool.execute(
        { taskId: "bash-regex", pattern: { regex: "ready: \\d+" } },
        createMockSdkContext(),
      );
      expect(result).toContain('matched "ready: 4242" in stdout at offset 4');
      expect(calls.filter((call) => call.cmd === "bash_regex_match")).toEqual([
        expect.objectContaining({
          params: expect.objectContaining({ pattern: "ready: \\d+", text: "" }),
        }),
        expect.objectContaining({
          params: expect.objectContaining({ pattern: "ready: \\d+", text: "abc ready: 4242\n" }),
        }),
      ]);
    } finally {
      await rm(join(outputPath, ".."), { recursive: true, force: true });
    }
  });

  test("bash_watch pattern regex surfaces invalid_regex as invalid_request", async () => {
    const outputPath = await spill("abc ready\n");
    try {
      const { watchTool } = makeCtx((cmd) => {
        if (cmd === "bash_regex_match") {
          return { success: false, code: "invalid_regex", message: "unclosed group" };
        }
        return {
          success: true,
          status: "running",
          mode: "pipes",
          output_path: outputPath,
        };
      });

      await expect(
        watchTool.execute(
          { taskId: "bash-regex-invalid", pattern: { regex: "(" } },
          createMockSdkContext(),
        ),
      ).rejects.toThrow("invalid_request: invalid_regex: unclosed group");
    } finally {
      await rm(join(outputPath, ".."), { recursive: true, force: true });
    }
  });

  test("bash_watch on already-terminal task returns immediately with reason exited", async () => {
    const { calls, watchTool } = makeCtx(() => ({
      success: true,
      status: "completed",
      exit_code: 0,
      duration_ms: 12,
      output_preview: "done",
    }));
    const result = await watchTool.execute({ taskId: "bash-done" }, createMockSdkContext());
    expect(result).toContain("task exited (completed, exit 0)");
    expect(result).toContain("done");
    expect(calls).toHaveLength(1);
    expect(calls[0].options?.keepBridgeOnTimeout).toBe(true);
    expect(calls[0].options?.transportTimeoutMs).toBe(30_000);
  });

  test("bash_watch on running task that completes mid-poll returns reason exited", async () => {
    let polls = 0;
    const { watchTool } = makeCtx(() => {
      polls += 1;
      return polls === 1
        ? { success: true, status: "running" }
        : { success: true, status: "completed", exit_code: 0, output_preview: "finished" };
    });
    const result = await watchTool.execute(
      { taskId: "bash-mid", timeoutMs: 500 },
      createMockSdkContext(),
    );
    expect(result).toContain("task exited (completed, exit 0)");
    expect(polls).toBe(2);
  });

  test("bash_watch timeoutMs returns timeout when pattern never matches", async () => {
    const outputPath = await spill("not yet\n");
    try {
      const { watchTool } = makeCtx(() => ({
        success: true,
        status: "running",
        mode: "pipes",
        output_path: outputPath,
      }));
      const result = await watchTool.execute(
        { taskId: "bash-timeout", pattern: "never", timeoutMs: 1 },
        createMockSdkContext(),
      );
      expect(result).toContain("timeout reached without match");
    } finally {
      await rm(join(outputPath, ".."), { recursive: true, force: true });
    }
  });

  test("bash_watch pattern + exit race scans terminal output before returning exited", async () => {
    const outputPath = await spill("pattern exists and match wins\n");
    try {
      const { watchTool } = makeCtx(() => ({
        success: true,
        status: "completed",
        exit_code: 0,
        mode: "pipes",
        output_path: outputPath,
      }));
      const result = await watchTool.execute(
        { taskId: "bash-race", pattern: "pattern" },
        createMockSdkContext(),
      );
      expect(result).toContain('matched "pattern" in stdout at offset 0');
      expect(result).not.toContain("task exited");
    } finally {
      await rm(join(outputPath, ".."), { recursive: true, force: true });
    }
  });

  test("bash_watch on PIPED bash with pattern reads output_path and matches", async () => {
    const outputPath = await spill("one\ntwo\nthree\n");
    try {
      const { watchTool } = makeCtx(() => ({
        success: true,
        status: "running",
        mode: "pipes",
        output_path: outputPath,
      }));
      const result = await watchTool.execute(
        { taskId: "bash-piped", pattern: "two" },
        createMockSdkContext(),
      );
      expect(result).toContain('matched "two" in stdout at offset 4');
    } finally {
      await rm(join(outputPath, ".."), { recursive: true, force: true });
    }
  });

  test("bash_watch on PIPED bash scans stderr_path as well as output_path", async () => {
    const spill = await spillPair("stdout\n", "warning: READY on stderr\n");
    try {
      const metadata = mock(() => {});
      const { watchTool } = makeCtx(() => ({
        success: true,
        status: "running",
        mode: "pipes",
        output_path: spill.stdoutPath,
        stderr_path: spill.stderrPath,
      }));
      const result = await watchTool.execute(
        { taskId: "bash-stderr", pattern: "READY" },
        createMockSdkContext({ metadata }),
      );
      expect(result).toContain('matched "READY" in stderr at offset 9');
      expect(metadata.mock.calls.at(-1)?.[0].waited).toMatchObject({
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
      const metadata = mock(() => {});
      const { watchTool } = makeCtx(() => ({
        success: true,
        status: "completed",
        exit_code: 0,
        mode: "pipes",
        output_path: spill.stdoutPath,
        stderr_path: spill.stderrPath,
      }));

      const result = await watchTool.execute(
        { taskId: "bash-stream-boundary", pattern: "ERROR" },
        createMockSdkContext({ metadata }),
      );

      expect(result).toContain("task exited (completed, exit 0)");
      expect(result).not.toContain("matched");
      expect(metadata.mock.calls.at(-1)?.[0].waited).toMatchObject({ reason: "exited" });
    } finally {
      await rm(spill.dir, { recursive: true, force: true });
    }
  });

  test("bash_watch preserves a same-stream match split across poll reads", async () => {
    const spill = await spillPair("RE", "");
    let polls = 0;
    try {
      const metadata = mock(() => {});
      const { watchTool } = makeCtx(async (command) => {
        if (command === "bash_status") {
          polls += 1;
          if (polls === 2) await appendFile(spill.stdoutPath, "ADY");
        }
        return {
          success: true,
          status: "running",
          mode: "pipes",
          output_path: spill.stdoutPath,
          stderr_path: spill.stderrPath,
        };
      });

      const result = await watchTool.execute(
        { taskId: "bash-same-stream-boundary", pattern: "READY", timeoutMs: 500 },
        createMockSdkContext({ metadata }),
      );

      expect(result).toContain('matched "READY" in stdout at offset 0');
      expect(metadata.mock.calls.at(-1)?.[0].waited).toMatchObject({
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

  test("bash_watch exit wait consumes pending completion to suppress duplicate reminder", async () => {
    __resetBgNotificationStateForTests();
    try {
      trackBgTask("s-consume", "bash-consume");
      const { watchTool } = makeCtx(() => ({
        success: true,
        status: "completed",
        exit_code: 0,
        bg_completions: [
          { task_id: "bash-consume", status: "completed", exit_code: 0, command: "echo done" },
        ],
      }));
      await watchTool.execute(
        { taskId: "bash-consume" },
        createMockSdkContext({ sessionID: "s-consume" }),
      );
      expect(sessionBgStates.get("s-consume")?.pendingCompletions).toEqual([]);
    } finally {
      __resetBgNotificationStateForTests();
    }
  });

  test("bash_kill forwards task_id and returns confirmation", async () => {
    const { killTool, calls } = makeCtx(() => ({ success: true, status: "killed" }));
    const result = await killTool.execute({ taskId: "bash-deadbeef" }, createMockSdkContext());
    expect(result).toBe("Task bash-deadbeef: killed");
    expect(calls[0].cmd).toBe("bash_kill");
    expect(calls[0].params.task_id).toBe("bash-deadbeef");
    expect(calls[0].options?.keepBridgeOnTimeout).toBe(true);
    expect(calls[0].options?.transportTimeoutMs).toBe(30_000);
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

  // ─── sync-watch user-message abort ───
  test("bash_watch sync wait aborts on user message and converts to async (with pattern)", async () => {
    __resetBgNotificationStateForTests();
    __resetSyncWatchAbortForTests();
    const sessionId = "s-abort-pattern";
    let pollCount = 0;
    const { calls, watchTool } = makeCtx((cmd) => {
      if (cmd === "bash_notify") return { success: true, watch_id: "watch-aborted" };
      pollCount++;
      // Signal abort after the first poll
      if (pollCount === 1) signalSyncWatchAbort(sessionId);
      return { success: true, status: "running", mode: "pipes" };
    });
    const result = await watchTool.execute(
      { taskId: "bash-abort", pattern: "READY" },
      createMockSdkContext({ sessionID: sessionId }),
    );
    // Should contain the conversion message
    expect(result).toContain("interrupted because you sent a message");
    expect(result).toContain("converted to an async watch");
    expect(result).toContain("watch-aborted");
    // Should have called bash_notify to register the async watch
    const notifyCall = calls.find((c) => c.cmd === "bash_notify");
    expect(notifyCall).toBeDefined();
    expect(notifyCall?.params.task_id).toBe("bash-abort");
    expect(notifyCall?.params.pattern).toBe("READY");
  });

  test("bash_watch sync wait aborts on user message without pattern (exit-only)", async () => {
    __resetBgNotificationStateForTests();
    __resetSyncWatchAbortForTests();
    const sessionId = "s-abort-no-pattern";
    let pollCount = 0;
    const { calls, watchTool } = makeCtx(() => {
      pollCount++;
      if (pollCount === 1) signalSyncWatchAbort(sessionId);
      return { success: true, status: "running", mode: "pipes" };
    });
    const result = await watchTool.execute(
      { taskId: "bash-abort-exit" },
      createMockSdkContext({ sessionID: sessionId }),
    );
    // Should contain the conversion message mentioning auto-reminder
    expect(result).toContain("interrupted because you sent a message");
    expect(result).toContain("completion reminder will be delivered automatically");
    // Should NOT have called bash_notify (no pattern = auto-reminder handles it)
    const notifyCall = calls.find((c) => c.cmd === "bash_notify");
    expect(notifyCall).toBeUndefined();
  });

  test("bash_watch stale abort flag is cleared at wait start", async () => {
    __resetBgNotificationStateForTests();
    __resetSyncWatchAbortForTests();
    const sessionId = "s-stale-abort";
    // Set a stale flag before the wait starts
    signalSyncWatchAbort(sessionId);
    const { watchTool } = makeCtx(() => ({
      success: true,
      status: "completed",
      exit_code: 0,
      duration_ms: 5,
    }));
    const result = await watchTool.execute(
      { taskId: "bash-stale" },
      createMockSdkContext({ sessionID: sessionId }),
    );
    // Should return normally (task exited), not abort
    expect(result).toContain("task exited");
    expect(result).not.toContain("interrupted");
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
// The subagent client sends block_to_completion so the server keeps the
// bash call inline until the command reaches a terminal state, regardless of
// how long it takes.
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
  config: PluginContext["config"] = {} as PluginContext["config"],
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
    config,
    storageDir: "/tmp/aft-test",
  };
  return { calls, tool: createBashTool(ctx) };
}

describe("OpenCode bash adapter — subagent gating", () => {
  test("subagent + background: true is silently converted to server block-to-completion", async () => {
    _resetSubagentCacheForTest();
    const { calls, tool: bash } = createSubagentHarness(() => ({
      success: true,
      status: "completed",
      task_id: "bash-conv",
      exit_code: 0,
      output: "converted output",
      truncated: false,
    }));
    const result = bashText(
      await bash.execute(
        { command: "sleep 30", background: true, timeout: 30_000 },
        createMockSdkContext({ sessionID: "ses_subagent_a" }),
      ),
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
    expect(bashCall?.params.block_to_completion).toBe(true);
    // Subagents must not make client-side bash_status or bash_promote calls
    // even if the caller asked for background:true. The server owns waiting for
    // the command to reach a terminal state.
    expect(calls.map((c) => c.command)).toEqual(["bash"]);
    expect(calls.find((c) => c.command === "bash_promote")).toBeUndefined();
  });

  test("subagent forced foreground does not ask the client to promote", async () => {
    _resetSubagentCacheForTest();
    const { calls, tool: bash } = createSubagentHarness(() => ({
      success: true,
      status: "completed",
      task_id: "bash-no-promote",
      exit_code: 0,
      output: "finished inline",
      truncated: false,
    }));

    const result = bashText(
      await bash.execute(
        { command: "slow-subagent", timeout: 0 },
        createMockSdkContext({ sessionID: "ses_subagent_deadline" }),
      ),
    );

    expect(result as string).toContain("finished inline");
    expect(calls.map((c) => c.command)).toEqual(["bash"]);
    expect(calls[0].params.block_to_completion).toBe(true);
    expect(calls.find((c) => c.command === "bash_promote")).toBeUndefined();
  });

  test("subagent + foreground delegates inline waiting to the server", async () => {
    _resetSubagentCacheForTest();
    const { calls, tool: bash } = createSubagentHarness(() => ({
      success: true,
      status: "completed",
      task_id: "bash-sub",
      exit_code: 0,
      output: "ok",
      truncated: false,
    }));
    const result = bashText(
      await bash.execute(
        { command: "fast-test", timeout: 30_000 },
        createMockSdkContext({ sessionID: "ses_subagent_b" }),
      ),
    );
    expect(typeof result).toBe("string");
    expect(result as string).not.toContain("promoted to background");
    expect(calls.map((c) => c.command)).toEqual(["bash"]);
    expect(calls[0].params.block_to_completion).toBe(true);
    // bash_promote should NEVER have been called for a subagent
    expect(calls.find((c) => c.command === "bash_promote")).toBeUndefined();
  });

  test("subagent + foreground without explicit timeout sizes transport to the hard cap", async () => {
    _resetSubagentCacheForTest();
    const { calls, tool: bash } = createSubagentHarness(() => ({
      success: true,
      status: "completed",
      task_id: "bash-sub2",
      exit_code: 0,
      output: "ok",
      truncated: false,
    }));
    bashText(
      await bash.execute(
        { command: "fast-test" }, // No user timeout, so bridge timeout is 30 minutes plus 10s margin.
        createMockSdkContext({ sessionID: "ses_subagent_c" }),
      ),
    );
    expect(calls[0].options?.transportTimeoutMs).toBe(30 * 60 * 1000 + 10_000);
    expect(calls.find((c) => c.command === "bash_promote")).toBeUndefined();
  });

  test("primary session + background: true still works (regression check)", async () => {
    _resetSubagentCacheForTest();
    // No client.session.get → resolveIsSubagent returns false → primary path.
    const { calls, tool: bash } = createHarness((command) => {
      if (command === "bash")
        return {
          success: true,
          status: "running",
          task_id: "bash-bg",
          output:
            "Background task started: bash-bg. A completion reminder will be delivered automatically; don't poll bash_status.",
        };
      return { success: true };
    });
    const result = bashText(
      await bash.execute(
        { command: "sleep 30", background: true },
        createMockSdkContext({ sessionID: "ses_primary_a" }),
      ),
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
              return {
                success: true,
                status: "running",
                task_id: "bash-err",
                output:
                  "Background task started: bash-err. A completion reminder will be delivered automatically; don't poll bash_status.",
              };
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
    const result = bashText(
      await bash.execute(
        { command: "sleep 30", background: true },
        createMockSdkContext({ sessionID: "ses_err_a" }),
      ),
    );
    // SDK failed → defaulted to primary → background: true succeeded
    expect(result as string).not.toContain("not allowed for subagents");
    expect(result as string).toContain("bash-err");
  });

  test("subagent_background true allows real background launch with guidance", async () => {
    _resetSubagentCacheForTest();
    const { calls, tool: bash } = createSubagentHarness(
      (command) => {
        if (command === "bash")
          return {
            success: true,
            status: "running",
            task_id: "bash-sub-bg",
            output:
              "Background task started: bash-sub-bg. A completion reminder will be delivered automatically; don't poll bash_status.",
          };
        return { success: true };
      },
      undefined,
      { bash: { subagent_background: true } } as PluginContext["config"],
    );
    const result = bashText(
      await bash.execute(
        { command: "sleep 30", background: true },
        createMockSdkContext({ sessionID: "ses_subagent_bg" }),
      ),
    );
    expect(result as string).toContain("Background task started: bash-sub-bg");
    expect(result as string).toContain('bash_watch({ taskId: "bash-sub-bg", timeoutMs: 60000 })');
    expect(calls.find((c) => c.command === "bash")?.params.background).toBe(true);
    expect(calls.find((c) => c.command === "bash")?.params.notify_on_completion).toBe(true);
  });

  test("subagent auto-promotion with subagent_background true includes guidance", async () => {
    _resetSubagentCacheForTest();
    const { calls, tool: bash } = createSubagentHarness(
      () => ({
        success: true,
        status: "running",
        task_id: "bash-sub-promote",
        output: `Foreground bash didn't finish within 0s and was promoted to background: bash-sub-promote. A completion reminder will be delivered automatically; use bash_status({ taskId: "bash-sub-promote" }) to inspect output or bash_kill({ taskId: "bash-sub-promote" }) to terminate.`,
      }),
      undefined,
      { bash: { subagent_background: true } } as PluginContext["config"],
    );
    const result = await withEnv({ AFT_TEST_FOREGROUND_WAIT_MS: "0" }, async () =>
      bashText(
        await bash.execute(
          { command: "sleep 30" },
          createMockSdkContext({ sessionID: "ses_subagent_promote" }),
        ),
      ),
    );
    expect(result as string).toContain("promoted to background: bash-sub-promote");
    expect(result as string).toContain(
      'bash_watch({ taskId: "bash-sub-promote", timeoutMs: 60000 })',
    );
    expect(calls.map((c) => c.command)).toEqual(["bash"]);
  });
});

describe("bash tool description (agent-facing wording)", () => {
  test("prohibits bash code search and steers to aft_search when registered", () => {
    const desc = bashToolDescription(true, true, true);
    expect(desc).toContain("DO NOT use bash for code search");
    expect(desc).toContain("STOP");
    expect(desc).toContain("aft_search");
  });

  test("replaces zoom steering with read when zoom is disabled", () => {
    const description = bashToolDescription(true, true, true, true, false);
    expect(description).toContain("aft_search");
    expect(description).toContain("aft_outline instead");
    expect(description).not.toContain("aft_zoom");
  });

  test("steers to the grep tool when aft_search is not registered", () => {
    const desc = bashToolDescription(false, true, true);
    expect(desc).toContain("DO NOT use bash for code search");
    expect(desc).toContain("grep tool");
    expect(desc).not.toContain("aft_search");
  });

  test("contains no internal vocabulary agents don't care about", () => {
    for (const variant of [
      bashToolDescription(true, true, true),
      bashToolDescription(false, false, false),
    ]) {
      expect(variant.toLowerCase()).not.toContain("hoisted");
      expect(variant.toLowerCase()).not.toContain("rewrit");
      expect(variant.toLowerCase()).not.toContain("unified bash schema");
    }
  });

  test("compression sentence only appears when compression is on", () => {
    expect(bashToolDescription(true, true, true)).toContain("compressed: false");
    expect(bashToolDescription(true, false, true)).not.toContain("compressed");
  });

  test("background on is foreground-first and names the background+bash_watch anti-pattern", () => {
    const on = bashToolDescription(true, true, true);
    // Lead with foreground-returns-inline and the explicit wait knob for known long commands.
    expect(on).toContain("foreground");
    expect(on).toContain("wait: true");
    expect(on).toContain("auto-promote can remind you while you work");
    // Demote background: true to the parallel-work-only case.
    expect(on).toContain("other useful work to do while it runs");
    // Name the exact anti-pattern this description previously caused.
    expect(on).toContain("never background a command and immediately bash_watch it");
    // Still forbid the bash_status polling loop.
    expect(on).toContain("never loop bash_status to wait");
  });

  test("bash_watch description recommends short sync waits and the completion reminder", () => {
    const ctx: PluginContext = {
      pool: {
        getBridge: () => ({ send: async () => ({ success: true }) }),
      } as unknown as BridgePool,
      client: createMockClient(),
      config: {} as PluginContext["config"],
      storageDir: "/tmp/aft-test",
    };
    const desc = createBashWatchTool(ctx).description;
    expect(desc).toContain("short remaining wait on a task");
    expect(desc).toContain("120s by default");
    expect(desc).toContain("bash({background:true})");
    expect(desc).toContain("Never loop bash_status");
  });

  test("bash_status description forbids polling loops", () => {
    const ctx: PluginContext = {
      pool: {
        getBridge: () => ({ send: async () => ({ success: true }) }),
      } as unknown as BridgePool,
      client: createMockClient(),
      config: {} as PluginContext["config"],
      storageDir: "/tmp/aft-test",
    };
    const desc = createBashStatusTool(ctx).description;
    expect(desc).toContain("never loop");
    expect(desc).toContain("To wait, use bash_watch");
  });

  test("bash_watch timeoutMs arg documents the configured cap", () => {
    const ctx: PluginContext = {
      pool: {
        getBridge: () => ({ send: async () => ({ success: true }) }),
      } as unknown as BridgePool,
      client: createMockClient(),
      config: {} as PluginContext["config"],
      storageDir: "/tmp/aft-test",
    };
    const watch = createBashWatchTool(ctx);
    const wrapped = tool.schema.object(watch.args);
    const jsonSchema = tool.schema.toJSONSchema(wrapped, { io: "input" }) as {
      properties?: { timeoutMs?: { maximum?: number; description?: string } };
    };
    expect(jsonSchema.properties?.timeoutMs?.maximum).toBe(1_800_000);
    expect(jsonSchema.properties?.timeoutMs?.description).toContain("bash.watch_sync_max_ms");
  });

  test("background/PTY sentences track bash.background config", () => {
    const on = bashToolDescription(true, true, true);
    expect(on).toContain("background: true");
    expect(on).toContain("pty: true");
    expect(on).toContain("bash_watch");
    // Background off: foreground commands block to completion, so do not
    // advertise promotion or any background-control tools.
    const off = bashToolDescription(true, true, false);
    expect(off).toContain("foreground to completion");
    expect(off).not.toContain("background: true");
    expect(off).not.toContain("pty: true");
    expect(off).not.toContain("promoted");
    expect(off).not.toContain("bash_status");
    expect(off).not.toContain("bash_kill");
    expect(off).not.toContain("bash_watch");
  });
});
