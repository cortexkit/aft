/// <reference path="../bun-test.d.ts" />
import { describe, expect, mock, spyOn, test } from "bun:test";
import { appendFile, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import {
  BASH_HOST_FALLBACK_BANNER,
  BASH_RUNON_DESCRIPTION,
  type BridgePool,
  type BridgeRequestOptions,
  BridgeTransportUnavailableError,
  watchClock,
  watchTimeoutSteer,
} from "@cortexkit/aft-bridge";
import { type ToolContext, tool } from "@opencode-ai/plugin";
import { withEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import {
  __resetBgNotificationStateForTests,
  sessionBgStates,
  trackBgTask,
} from "../bg-notifications.js";
import * as logger from "../logger.js";
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

/**
 * The abort retry runs detached from the tool call, so its outcome line can be
 * written after `execute` has already returned. Wait a bounded time for it.
 */
async function abortOutcomeLog(
  spy: ReturnType<typeof spyOn<typeof logger, "sessionLog">>,
  sessionID: string,
): Promise<{ message: string; data: unknown }> {
  // Filter on the test's own session: an earlier test's abort loop keeps
  // running after its call settles and can log its outcome while this test's
  // spy is installed, and every other test shares one session id.
  for (let attempt = 0; attempt < 200; attempt++) {
    const call = spy.mock.calls.find(
      ([session, message]) =>
        session === sessionID && message.startsWith("[bash] foreground abort "),
    );
    if (call) return { message: call[1], data: call[2] };
    await Bun.sleep(5);
  }
  throw new Error("foreground abort outcome was never logged");
}

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
  return { calls, ctx, tool: createBashTool(ctx, aftSearchRegistered) };
}

function safeParse(schema: unknown, value: unknown): { success: boolean } {
  return (schema as SafeParseSchema).safeParse(value);
}

describe("OpenCode bash adapter", () => {
  test("live runon safety switch updates the schema and refuses a stale call", async () => {
    const {
      tool: bash,
      ctx,
      calls,
    } = createHarness(() => ({ success: true, output: "" }), undefined, false, {
      subc: { connection_file: "/run/subc-connection.json" },
      remote_exec: { enabled: true },
    } as PluginContext["config"]);
    expect(bash.args.runon).toBeUndefined();
    ctx.config = { ...ctx.config, bash: { runon_enabled: true } };
    expect(bash.args.runon).toBeDefined();
    ctx.config = { ...ctx.config, bash: { runon_enabled: false } };
    expect(bash.args.runon).toBeUndefined();
    expect(bash.description).not.toContain("runon");
    await expect(
      bash.execute({ command: "echo never", runon: "linux" }, createMockSdkContext()),
    ).rejects.toThrow("runon refused");
    expect(calls).toHaveLength(0);
  });
  test("schema accepts valid unified bash params and rejects invalid shapes", () => {
    // `sandbox` is only offered while the native sandbox is enabled.
    const { tool: bash } = createHarness(() => ({ success: true, output: "" }), undefined, false, {
      sandbox: { enabled: true },
    } as PluginContext["config"]);

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

  test("bash runon schema and guidance are absent without available remote execution", () => {
    for (const config of [
      {},
      { subc: { connection_file: "/run/subc-connection.json" } },
      { remote_exec: { enabled: true } },
      {
        subc: { connection_file: "/run/subc-connection.json" },
        remote_exec: { enabled: true, project_off: true },
      },
    ]) {
      const bash = createHarness(() => ({ success: true, output: "" }), undefined, false, {
        disabled_tools: [],
        ...config,
      } as PluginContext["config"]).tool;
      expect(bash.args.runon).toBeUndefined();
      expect(bash.description).not.toContain("runon");
    }
  });

  test.skipIf(process.platform === "win32")(
    "bash runon schema and guidance are present with available remote execution",
    () => {
      const bash = createHarness(() => ({ success: true, output: "" }), undefined, false, {
        subc: { connection_file: "/run/subc-connection.json" },
        remote_exec: { enabled: true },
        bash: { runon_enabled: true },
      } as PluginContext["config"]).tool;
      expect(bash.args.runon).toBeDefined();
      expect(bash.description).toContain('When remote runs are available, put `runon: "linux"`');
      expect(bash.description).toContain("including chains and pipes");
      expect(bash.description).toContain(
        "Keep git, gh, interactive and file-editing commands local, and keep a line local if it needs macOS (Seatbelt, codesign, launchd, TCC, AppKit)",
      );
      expect(bash.description).toContain(
        "or runs binaries built on this machine: a remote build leaves no binaries or target/ output here.",
      );
    },
  );

  // Remote task dispatch is Unix-only; a user switch cannot make it usable on Windows.
  test.skipIf(process.platform !== "win32")(
    "Windows bash omits runon even with daemon mode and enabled user config",
    () => {
      const bash = createHarness(() => ({ success: true, output: "" }), undefined, false, {
        subc: { connection_file: "/run/subc-connection.json" },
        remote_exec: { enabled: true },
        bash: { runon_enabled: true },
      } as PluginContext["config"]).tool;
      expect(bash.args.runon).toBeUndefined();
      expect(bash.description).not.toContain("runon");
    },
  );

  test.skipIf(process.platform === "win32")(
    "runon is offered only in subc mode with remote runs enabled in the user config",
    () => {
      const offered = (config: Record<string, unknown>) =>
        "runon" in
        createHarness(() => ({ success: true, output: "" }), undefined, false, {
          disabled_tools: [],
          ...config,
        } as PluginContext["config"]).tool.args;
      const subc = {
        subc: { connection_file: "/run/subc-connection.json" },
        bash: { runon_enabled: true },
      };
      expect(offered({ ...subc, remote_exec: { enabled: true } })).toBe(true);
      // No user-tier switch, or standalone transport: never offered.
      expect(offered({ ...subc })).toBe(false);
      expect(offered({ ...subc, remote_exec: { enabled: false } })).toBe(false);
      expect(offered({ remote_exec: { enabled: true } })).toBe(false);
      // The project turned remote runs off.
      expect(offered({ ...subc, remote_exec: { enabled: false, project_off: true } })).toBe(false);
      const { tool: bash } = createHarness(
        () => ({ success: true, output: "" }),
        undefined,
        false,
        {
          ...subc,
          remote_exec: { enabled: true },
          bash: { runon_enabled: true },
        } as PluginContext["config"],
      );
      expect(safeParse(bash.args.runon, "linux").success).toBe(true);
      const jsonSchema = tool.schema.toJSONSchema(bash.args.runon as never, { io: "input" }) as {
        description?: string;
      };
      expect(jsonSchema.description).toBe(BASH_RUNON_DESCRIPTION);
    },
  );

  test("runon is forwarded to the engine and never becomes a host-fallback run", async () => {
    const { calls, tool: bash } = createHarness(
      () => ({ success: true, output: "ran remotely on ck-motor\nok", exit_code: 0 }),
      undefined,
      false,
      {
        subc: { connection_file: "/run/subc-connection.json" },
        remote_exec: { enabled: true },
        bash: { runon_enabled: true },
      } as PluginContext["config"],
    );
    await bash.execute(
      { command: "FOO=1 cargo test | tail -1", runon: "linux" },
      createMockSdkContext({}),
    );
    expect(calls[0].params).toMatchObject({
      command: "FOO=1 cargo test | tail -1",
      runon: "linux",
    });

    const dead = createHarness(
      () => {
        throw new BridgeTransportUnavailableError("transport down");
      },
      undefined,
      false,
      {
        bash: { host_fallback: true, runon_enabled: true },
        subc: { connection_file: "/run/subc-connection.json" },
        remote_exec: { enabled: true },
      } as PluginContext["config"],
    );
    await expect(
      dead.tool.execute({ command: "printf no", runon: "linux" }, createMockSdkContext({})),
    ).rejects.toThrow("runon is unsupported, and the command was not run locally");
  });

  test("schema omits wait, background and PTY args when bash.background is disabled", () => {
    const { tool: bash } = createHarness(() => ({ success: true, output: "" }), undefined, false, {
      bash: { background: false },
    } as PluginContext["config"]);

    // `wait` only changes whether a command may auto-promote to the
    // background, so it leaves with the other background arguments. `sandbox`
    // is absent too: this config does not enable the native sandbox.
    expect(Object.keys(bash.args)).toEqual([
      "command",
      "timeout",
      "workdir",
      "description",
      "compressed",
    ]);
    expect(bash.args.wait).toBeUndefined();
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
        `AFT UNAVAILABLE (transport down) - host fallback execution:\n\nExact command:\n${command}\n\nWorking directory:\n${PROJECT_CWD}`,
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

  test("foreground forwards a timeout shorter than the wait window as the hard kill cap", async () => {
    // `timeout` is a hard kill cap in every mode. A cap below the foreground
    // wait window used to be dropped here, so the engine applied its
    // 30-minute default and a command meant to die after 100 ms ran to
    // completion. The engine kills it at the cap and answers timed out, and
    // the wait window stays the configured one rather than shrinking to the
    // cap, so forwarding the cap verbatim is safe.
    const { calls, tool: bash } = createHarness(() => ({
      success: true,
      output: "done",
      exit_code: 0,
      truncated: false,
    }));

    bashText(await bash.execute({ command: "echo hi", timeout: 100 }, createMockSdkContext()));

    expect(calls[0].command).toBe("bash");
    expect(calls[0].params.timeout).toBe(100);
    expect(calls[0].params.wait).toBe(false);
    expect(calls[0].params.background).toBe(false);
  });

  test("foreground forwards a numeric-string timeout as the hard kill cap", async () => {
    const { calls, tool: bash } = createHarness(() => ({
      success: true,
      output: "done",
      exit_code: 0,
      truncated: false,
    }));

    bashText(
      await bash.execute(
        { command: "echo hi", timeout: "1000" as unknown as number },
        createMockSdkContext(),
      ),
    );

    expect(calls[0].params.timeout).toBe(1000);
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

  test("foreground abort waits until Rust registers the task", async () => {
    let abortAttempts = 0;
    let settleBash: ((response: BridgeResponse) => void) | undefined;
    const bashResponse = new Promise<BridgeResponse>((resolvePromise) => {
      settleBash = resolvePromise;
    });
    const { calls, tool: bash } = createHarness((command) => {
      if (command === "bash") return bashResponse;
      if (command === "bash_abort_inflight") {
        abortAttempts += 1;
        if (abortAttempts < 3) return { success: true, killed: 0 };
        settleBash?.({
          success: true,
          status: "killed",
          task_id: "task-late-registration",
          output: "",
          truncated: false,
        });
        return { success: true, killed: 1 };
      }
      throw new Error(`unexpected command ${command}`);
    });
    const controller = new AbortController();

    const result = bash.execute(
      { command: "sleep 30" },
      createMockSdkContext({ abort: controller.signal }),
    );
    controller.abort();

    await result;
    expect(abortAttempts).toBe(3);
    expect(calls.filter((call) => call.command === "bash")).toHaveLength(1);
    expect(calls.filter((call) => call.command === "bash_abort_inflight")).toHaveLength(3);
  });

  test("foreground abort logs every attempt's answer when it kills the task", async () => {
    const logSpy = spyOn(logger, "sessionLog");
    try {
      let abortAttempts = 0;
      let settleBash: ((response: BridgeResponse) => void) | undefined;
      const bashResponse = new Promise<BridgeResponse>((resolvePromise) => {
        settleBash = resolvePromise;
      });
      const { tool: bash } = createHarness((command) => {
        if (command === "bash") return bashResponse;
        abortAttempts += 1;
        if (abortAttempts < 3) return { success: true, killed: 0 };
        settleBash?.({ success: true, status: "killed", task_id: "t", output: "" });
        return { success: true, killed: 1 };
      });
      const controller = new AbortController();
      // A leftover line from another session, exactly the shape an earlier
      // test's still-running abort loop writes. The helper must skip it.
      logger.sessionLog("test-session", "[bash] foreground abort killed", {
        attempts: 1,
        results: ["killed=1"],
      });
      const result = bash.execute(
        { command: "sleep 30" },
        createMockSdkContext({ abort: controller.signal, sessionID: "abort-log-killed" }),
      );
      controller.abort();
      await result;

      const outcome = await abortOutcomeLog(logSpy, "abort-log-killed");
      expect(outcome.message).toBe("[bash] foreground abort killed");
      expect(outcome.data).toMatchObject({
        attempts: 3,
        results: ["killed=0", "killed=0", "killed=1"],
      });
    } finally {
      logSpy.mockRestore();
    }
  });

  test("foreground abort that finds nothing to kill still leaves a trace", async () => {
    const logSpy = spyOn(logger, "sessionLog");
    try {
      let abortAttempts = 0;
      let settleBash: ((response: BridgeResponse) => void) | undefined;
      const bashResponse = new Promise<BridgeResponse>((resolvePromise) => {
        settleBash = resolvePromise;
      });
      const { tool: bash } = createHarness((command) => {
        if (command === "bash") return bashResponse;
        abortAttempts += 1;
        // The call ends by itself while Rust still reports nothing to kill.
        if (abortAttempts === 2) {
          settleBash?.({
            success: true,
            status: "completed",
            task_id: "t",
            exit_code: 0,
            output: "",
          });
        }
        return { success: true, killed: 0 };
      });
      const controller = new AbortController();
      const result = bash.execute(
        { command: "sleep 30" },
        createMockSdkContext({ abort: controller.signal, sessionID: "abort-log-settled" }),
      );
      controller.abort();
      await result;

      const outcome = await abortOutcomeLog(logSpy, "abort-log-settled");
      expect(outcome.message).toBe("[bash] foreground abort call_settled");
      expect(outcome.data).toMatchObject({
        attempts: 2,
        results: ["killed=0", "killed=0"],
        last_response: { killed: 0 },
      });
    } finally {
      logSpy.mockRestore();
    }
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

  test("stale wait, background and pty arguments are ignored, never rejected, when background is off", async () => {
    // These arguments are not in the schema with background off, but a model
    // replaying an older call can still send them. Paired, they would trip the
    // wait/background contradiction checks; alone, `wait: true` would make the
    // call detachable into a background task.
    const { calls, tool: bash } = createHarness(
      () => ({ success: true, status: "completed", exit_code: 0, output: "done" }),
      undefined,
      false,
      { bash: { background: false } } as PluginContext["config"],
    );

    for (const stale of [
      { wait: true, background: true },
      { wait: true, pty: true },
      { wait: true },
    ]) {
      const output = bashText(
        await bash.execute(
          { command: "true", ...stale },
          createMockSdkContext({ sessionID: "no-bg-stale" }),
        ),
      );
      expect(output).toBe("done");
      const params = calls.at(-1)?.params ?? {};
      expect(params.wait).toBe(false);
      expect(params.background).toBe(false);
      expect(params.pty).toBe(false);
      expect(params.block_to_completion).toBe(true);
    }
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

  test("PTY incomplete capture status preserves command outcome and shows warning", async () => {
    const reason = "PTY output may be incomplete: output drain deadline expired before EOF";
    for (const exitCode of [0, 17]) {
      for (const outputMode of ["screen", "raw", "both"]) {
        const status = exitCode === 0 ? "completed" : "failed";
        const { statusTool } = makeCtx(() => ({
          success: true,
          status,
          exit_code: exitCode,
          mode: "pty",
          output_incomplete: true,
          status_reason: reason,
          pty_screen: "captured prefix",
          pty_raw: "captured prefix",
        }));
        const text = await statusTool.execute(
          { taskId: "bash-incomplete", outputMode },
          createMockSdkContext(),
        );
        expect(text).toContain(`Task bash-incomplete: ${status} (exit ${exitCode})`);
        expect(text).toContain(reason);
      }
    }
  });

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

  test("bash_watch reply names the bash.watch_sync_max_ms cap when the wait ran at the cap", async () => {
    _resetSubagentCacheForTest();
    // Only a primary session is bounded by the cap: it asks for longer than
    // the cap and is clamped to it. (A delegated worker's watch without a
    // timeout is bounded by the worker wait limit instead.)
    const { watchTool } = makeCtx(() => ({ success: true, status: "running" }), {
      bash: { watch_sync_max_ms: 1_000 },
    } as PluginContext["config"]);
    const result = await watchTool.execute(
      { taskId: "bash-at-cap", timeoutMs: 1_000 },
      createMockSdkContext({ sessionID: "ses_watch_at_cap" }),
    );
    expect(result).toMatch(
      /Waited \d+ms \(limit 1000ms, the bash\.watch_sync_max_ms cap\); timeout reached without match/,
    );
  });

  test("bash_watch timeout gives a delegated worker only the worker steer, with no cap", async () => {
    _resetSubagentCacheForTest();
    const { ctx, watchTool } = makeCtx(
      () => ({ success: true, status: "running", duration_ms: 4_000, output_preview: "tick\n" }),
      {
        bash: { watch_sync_max_ms: 90_000 },
      } as PluginContext["config"],
    );
    ctx.client = createSubagentClient();
    const result = await watchTool.execute(
      { taskId: "bash-worker-timeout", timeoutMs: 1 },
      createMockSdkContext({ sessionID: "ses_watch_worker_text" }),
    );
    expect(result).toContain("timeout reached without match");
    expect(result).toContain("The command is still running after");
    expect(result).toContain(
      'Call bash_watch({ taskId: "bash-worker-timeout" }) again to keep waiting',
    );
    expect(result).toContain("without timeoutMs a watch waits up to the worker wait limit");
    expect(result).toContain("It has run for 4s.");
    expect(result).toContain("Recent output:\ntick");
    expect(result).not.toContain("waits until the command finishes");
    expect(result).not.toContain("90000");
    expect(result).not.toContain("end your turn");
    expect(result).not.toContain("completion reminder");
  });

  test("bash_watch timeout gives a primary session only the primary steer", async () => {
    _resetSubagentCacheForTest();
    const { watchTool } = makeCtx(() => ({ success: true, status: "running" }));
    const result = await watchTool.execute(
      { taskId: "bash-primary-timeout", timeoutMs: 1 },
      createMockSdkContext({ sessionID: "ses_watch_primary_text" }),
    );
    expect(result).toContain("timeout reached without match");
    expect(result).toContain(watchTimeoutSteer());
    expect(result).not.toContain("don't report a result");
  });

  test("bash_watch without timeoutMs waits up to the worker wait limit for a worker and 30000 for a primary", async () => {
    const effectiveFor = async (
      subagent: boolean,
      sessionID: string,
      config?: PluginContext["config"],
    ) => {
      _resetSubagentCacheForTest();
      let polls = 0;
      const { ctx, watchTool } = makeCtx(() => {
        polls += 1;
        return polls === 1
          ? { success: true, status: "running" }
          : { success: true, status: "completed", exit_code: 0 };
      }, config);
      if (subagent) ctx.client = createSubagentClient();
      const metadata = mock((_data: Record<string, unknown>) => {});
      await watchTool.execute(
        { taskId: `bash-default-${sessionID}` },
        createMockSdkContext({ sessionID, metadata }),
      );
      return metadata.mock.calls.at(-1)?.[0].effectiveWaitMs;
    };
    expect(await effectiveFor(true, "ses_watch_worker_default")).toBe(1_800_000);
    // A configured limit is honoured.
    expect(
      await effectiveFor(true, "ses_watch_worker_configured", {
        bash: { worker_wait_max_ms: 300_000 },
      } as PluginContext["config"]),
    ).toBe(300_000);
    expect(await effectiveFor(false, "ses_watch_primary_default")).toBe(30_000);
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
    expect(result).toMatch(/after \d+ms of waiting/);
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
    expect(result).toMatch(/after \d+ms of waiting/);
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

  // ─── delegated worker: a watch without timeoutMs waits up to the worker wait limit ───
  //
  // These run on simulated time (useFakeWatchClock), so a wait of several
  // minutes finishes in milliseconds.

  test("a worker's bash_watch without timeoutMs returns when the task exits, past 120 s and before the limit", async () => {
    _resetSubagentCacheForTest();
    __resetSyncWatchAbortForTests();
    const clock = useFakeWatchClock();
    try {
      let polls = 0;
      const { ctx, watchTool } = makeCtx(() => {
        polls += 1;
        return clock.now() < 150_000
          ? { success: true, status: "running" }
          : { success: true, status: "completed", exit_code: 0 };
      });
      ctx.client = createSubagentClient();
      const result = await watchTool.execute(
        { taskId: "bash-worker-long" },
        createMockSdkContext({ sessionID: "ses_worker_long_watch" }),
      );
      expect(result).toContain("task exited (completed, exit 0)");
      expect(result).toContain("(limit 1800000ms)");
      expect(result).not.toContain("timeout reached");
      const waited = Number(/Waited (\d+)ms/.exec(result)?.[1]);
      expect(waited).toBeGreaterThanOrEqual(150_000);
      // The poll interval backs off, so a long wait stays cheap: at a fixed
      // 100 ms this wait would have taken 1500 status polls.
      expect(polls).toBeLessThan(500);
    } finally {
      clock.restore();
    }
  });

  // The worker wait limit: a stuck command once held a worker for fifteen
  // hours behind a watch with no deadline. At the limit the watch hands
  // control back, says the command is still running, and how to go on.
  test("a worker's bash_watch without timeoutMs returns still-running at the worker wait limit", async () => {
    _resetSubagentCacheForTest();
    __resetSyncWatchAbortForTests();
    const clock = useFakeWatchClock();
    try {
      const { ctx, watchTool } = makeCtx(() => ({
        success: true,
        status: "running",
        mode: "pipes",
        duration_ms: Math.round(clock.now()) + 20_000,
        output_preview: "(pass) one\n(pass) two\n",
      }));
      ctx.client = createSubagentClient();
      const result = await watchTool.execute(
        { taskId: "bash-worker-stuck" },
        createMockSdkContext({ sessionID: "ses_worker_watch_cap" }),
      );
      expect(result).toMatch(/Waited 1800000ms \(limit 1800000ms\); timeout reached without match/);
      expect(result).toContain("The command is still running after 30 minutes of watching");
      expect(result).toMatch(/It has run for 18\d\d(\.\d)?s\./);
      expect(result).toContain(
        'Call bash_watch({ taskId: "bash-worker-stuck" }) again to keep waiting',
      );
      expect(result).toContain('bash_kill({ taskId: "bash-worker-stuck" })');
      expect(result).toContain("Recent output:\n(pass) one\n(pass) two");
      expect(clock.now()).toBeLessThan(1_802_000);
    } finally {
      clock.restore();
    }
  });

  test("a worker bash_watch at its cap waits for a timeout just inside the handoff margin", async () => {
    _resetSubagentCacheForTest();
    __resetSyncWatchAbortForTests();
    const clock = useFakeWatchClock();
    const killAtMs = 1_802_000;
    const reason = "killed by the explicit timeout (exit 124)";
    try {
      const { ctx, watchTool } = makeCtx(() =>
        clock.now() < killAtMs
          ? {
              success: true,
              status: "running",
              mode: "pipes",
              started_at: Date.now(),
              hard_kill: { limit_ms: killAtMs, source: "timeout" },
              elapsed_ms: Math.round(clock.now()),
            }
          : { success: true, status: "timed_out", exit_code: 124, status_reason: reason },
      );
      ctx.client = createSubagentClient();
      const result = await watchTool.execute(
        { taskId: "bash-worker-timeout-at-cap" },
        createMockSdkContext({ sessionID: "ses_worker_timeout_at_cap" }),
      );
      expect(result).toContain("task exited (timed_out, exit 124)");
      expect(result).toContain("The task was killed by the explicit timeout (exit 124).");
      expect(result).not.toContain("timeout reached without match");
      expect(result).not.toContain("The command is still running");
      expect(clock.now()).toBeGreaterThanOrEqual(killAtMs);
    } finally {
      clock.restore();
    }
  });

  // A worker once watched a background task with "no limit", never learned
  // the task had AFT's 30-minute default kill, and took the kill for its
  // command failing. Every watch result names the task's own deadline, and a
  // kill by that default is named as such.
  test("a worker's watch names the task's kill deadline and, once it fires, the default limit that killed it", async () => {
    _resetSubagentCacheForTest();
    __resetSyncWatchAbortForTests();
    const clock = useFakeWatchClock();
    const reason =
      "killed by AFT's default background limit of 30 minutes (exit 124); the command itself did not fail, pass a longer `timeout` if it needs more time";
    try {
      const { ctx, watchTool } = makeCtx(() =>
        clock.now() < 1_000_000
          ? {
              success: true,
              status: "running",
              started_at: Date.now(),
              hard_kill: { limit_ms: 1_800_000, source: "default" },
            }
          : { success: true, status: "timed_out", exit_code: 124, status_reason: reason },
      );
      ctx.client = createSubagentClient();
      const context = createMockSdkContext({ sessionID: "ses_worker_watch_deadline" });
      const running = await watchTool.execute(
        { taskId: "bash-worker-deadline", timeoutMs: 60_000 },
        context,
      );
      expect(running).toContain("when it has run 30 minutes (its default background limit)");
      expect(running).toContain("remain.");
      expect(running).toContain("each wait you make on it moves that kill");
      const killed = await watchTool.execute({ taskId: "bash-worker-deadline" }, context);
      expect(killed).toContain("task exited (timed_out, exit 124)");
      expect(killed).toContain(
        "The task was killed by AFT's default background limit of 30 minutes (exit 124); the command itself did not fail",
      );
      expect(killed).not.toContain("no limit");
    } finally {
      clock.restore();
    }
  });

  test("a worker's second bash_watch keeps waiting and returns the result", async () => {
    _resetSubagentCacheForTest();
    __resetSyncWatchAbortForTests();
    const clock = useFakeWatchClock();
    try {
      // The task finishes 10 minutes into the second watch.
      const { ctx, watchTool } = makeCtx(() =>
        clock.now() < 2_400_000
          ? { success: true, status: "running" }
          : { success: true, status: "completed", exit_code: 0, output_preview: "all green\n" },
      );
      ctx.client = createSubagentClient();
      const context = createMockSdkContext({ sessionID: "ses_worker_watch_twice" });
      const first = await watchTool.execute({ taskId: "bash-worker-twice" }, context);
      expect(first).toContain("The command is still running after 30 minutes of watching");
      const second = await watchTool.execute({ taskId: "bash-worker-twice" }, context);
      expect(second).toContain("task exited (completed, exit 0)");
      expect(second).toContain("all green");
      expect(second).not.toContain("timeout reached");
    } finally {
      clock.restore();
    }
  });

  test("a worker's bash_watch honours a configured worker_wait_max_ms", async () => {
    _resetSubagentCacheForTest();
    __resetSyncWatchAbortForTests();
    const clock = useFakeWatchClock();
    try {
      const { ctx, watchTool } = makeCtx(() => ({ success: true, status: "running" }), {
        bash: { worker_wait_max_ms: 300_000 },
      } as PluginContext["config"]);
      ctx.client = createSubagentClient();
      const result = await watchTool.execute(
        { taskId: "bash-worker-configured" },
        createMockSdkContext({ sessionID: "ses_worker_watch_configured" }),
      );
      expect(result).toMatch(/Waited 300000ms \(limit 300000ms\); timeout reached without match/);
      expect(result).toContain("still running after 5 minutes of watching");
      expect(result).toContain("No output yet.");
    } finally {
      clock.restore();
    }
  });

  test("a new message ends a worker's watch before its limit", async () => {
    _resetSubagentCacheForTest();
    __resetBgNotificationStateForTests();
    __resetSyncWatchAbortForTests();
    const clock = useFakeWatchClock();
    const sessionId = "ses_worker_watch_message";
    try {
      const { ctx, watchTool } = makeCtx(() => {
        if (clock.now() >= 200_000) signalSyncWatchAbort(sessionId);
        return { success: true, status: "running", mode: "pipes" };
      });
      ctx.client = createSubagentClient();
      const result = await watchTool.execute(
        { taskId: "bash-worker-message" },
        createMockSdkContext({ sessionID: sessionId }),
      );
      expect(result).toContain("interrupted because you sent a message");
      expect(result).toContain("Call bash_watch again to keep waiting");
      expect(result).not.toContain("completion reminder");
      expect(clock.now()).toBeGreaterThanOrEqual(200_000);
      expect(clock.now()).toBeLessThan(202_000);
    } finally {
      clock.restore();
    }
  });

  test("an aborted tool call ends a worker's watch before its limit", async () => {
    _resetSubagentCacheForTest();
    __resetSyncWatchAbortForTests();
    const clock = useFakeWatchClock();
    const controller = new AbortController();
    try {
      const { ctx, watchTool } = makeCtx(() => {
        if (clock.now() >= 300_000) controller.abort();
        return { success: true, status: "running" };
      });
      ctx.client = createSubagentClient();
      const result = await watchTool.execute(
        { taskId: "bash-worker-abort" },
        createMockSdkContext({ sessionID: "ses_worker_watch_abort", abort: controller.signal }),
      );
      expect(result).toContain("the watch was cancelled");
      expect(clock.now()).toBeLessThan(302_000);
    } finally {
      clock.restore();
    }
  });

  test("a worker's explicit timeoutMs above the cap is honoured as given", async () => {
    _resetSubagentCacheForTest();
    __resetSyncWatchAbortForTests();
    const clock = useFakeWatchClock();
    try {
      const { ctx, watchTool } = makeCtx(() => ({ success: true, status: "running" }));
      ctx.client = createSubagentClient();
      const result = await watchTool.execute(
        { taskId: "bash-worker-explicit", timeoutMs: 600_000 },
        createMockSdkContext({ sessionID: "ses_worker_watch_explicit" }),
      );
      expect(result).toMatch(/Waited 600000ms \(limit 600000ms\); timeout reached without match/);
      expect(result).toContain("The command is still running after 10 minutes of watching");
    } finally {
      clock.restore();
    }
  });

  test("a primary's bash_watch without timeoutMs still ends at 30 s", async () => {
    _resetSubagentCacheForTest();
    __resetSyncWatchAbortForTests();
    const clock = useFakeWatchClock();
    try {
      const { watchTool } = makeCtx(() => ({ success: true, status: "running" }));
      const result = await watchTool.execute(
        { taskId: "bash-primary-default" },
        createMockSdkContext({ sessionID: "ses_primary_watch_default" }),
      );
      expect(result).toMatch(/Waited 30000ms \(limit 30000ms\); timeout reached without match/);
      expect(result).toContain(watchTimeoutSteer());
    } finally {
      clock.restore();
    }
  });

  test("a worker's bash_status of a running task points at bash_watch, not a reminder", async () => {
    _resetSubagentCacheForTest();
    const { ctx, statusTool } = makeCtx(() => ({ success: true, status: "running" }));
    ctx.client = createSubagentClient();
    const result = await statusTool.execute(
      { taskId: "bash-worker-status" },
      createMockSdkContext({ sessionID: "ses_worker_status" }),
    );
    expect(result).toContain("To wait for it, call bash_watch");
    expect(result).not.toContain("completion reminder");
  });
});

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
  test("subagent + background: true is honoured by default", async () => {
    _resetSubagentCacheForTest();
    const { calls, tool: bash } = createSubagentHarness(() => ({
      success: true,
      status: "running",
      task_id: "bash-bg",
      output: "started",
      truncated: false,
    }));
    const result = bashText(
      await bash.execute(
        { command: "sleep 30", background: true, timeout: 30_000 },
        createMockSdkContext({ sessionID: "ses_subagent_a" }),
      ),
    );
    expect(typeof result).toBe("string");
    expect(result as string).toContain("started");
    expect(result as string).toContain("bash-bg");
    const bashCall = calls.find((c) => c.command === "bash");
    expect(bashCall).toBeDefined();
    expect(bashCall?.params.background).toBe(true);
    expect(bashCall?.params.notify_on_completion).toBe(true);
    expect(bashCall?.params.block_to_completion).toBe(false);
    expect(calls.map((c) => c.command)).toEqual(["bash"]);
  });

  test("subagent + background: true is converted when explicitly disabled", async () => {
    _resetSubagentCacheForTest();
    const { calls, tool: bash } = createSubagentHarness(
      () => ({ success: true, status: "completed", output: "converted output" }),
      "ses_parent_xyz",
      { bash: { subagent_background: false } } as PluginContext["config"],
    );
    const result = bashText(
      await bash.execute(
        { command: "sleep 30", background: true, timeout: 30_000 },
        createMockSdkContext({ sessionID: "ses_subagent_disabled" }),
      ),
    );
    expect(result as string).toContain("converted output");
    expect(calls[0].params.background).toBe(false);
    expect(calls[0].params.notify_on_completion).toBe(false);
    expect(calls[0].params.block_to_completion).toBe(true);
  });

  test("subagent forced foreground does not ask the client to promote", async () => {
    _resetSubagentCacheForTest();
    const { calls, tool: bash } = createSubagentHarness(
      () => ({
        success: true,
        status: "completed",
        task_id: "bash-no-promote",
        exit_code: 0,
        output: "finished inline",
        truncated: false,
      }),
      "ses_parent_xyz",
      { bash: { subagent_background: false } } as PluginContext["config"],
    );

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
    const { calls, tool: bash } = createSubagentHarness(
      () => ({
        success: true,
        status: "completed",
        task_id: "bash-sub",
        exit_code: 0,
        output: "ok",
        truncated: false,
      }),
      "ses_parent_xyz",
      { bash: { subagent_background: false } } as PluginContext["config"],
    );
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
    const { calls, tool: bash } = createSubagentHarness(
      () => ({
        success: true,
        status: "completed",
        task_id: "bash-sub2",
        exit_code: 0,
        output: "ok",
        truncated: false,
      }),
      "ses_parent_xyz",
      { bash: { subagent_background: false } } as PluginContext["config"],
    );
    bashText(
      await bash.execute(
        { command: "fast-test" }, // No user timeout, so bridge timeout is 30 minutes plus 10s margin.
        createMockSdkContext({ sessionID: "ses_subagent_c" }),
      ),
    );
    expect(calls[0].options?.transportTimeoutMs).toBe(30 * 60 * 1000 + 10_000);
    expect(calls.find((c) => c.command === "bash_promote")).toBeUndefined();
  });

  test("subagent wait:true without a timeout gets a transport budget of the worker wait limit", async () => {
    _resetSubagentCacheForTest();
    const { calls, tool: bash } = createSubagentHarness(() => ({
      success: true,
      status: "completed",
      task_id: "bash-sub-wait",
      exit_code: 0,
      output: "built",
    }));
    await bash.execute(
      { command: "long-build", wait: true },
      createMockSdkContext({ sessionID: "ses_subagent_wait" }),
    );
    expect(calls[0].params.wait).toBe(true);
    expect(calls[0].params.timeout).toBeUndefined();
    expect(calls[0].params.worker_session).toBe(true);
    // The engine hands the call back at the worker wait limit (30 minutes
    // by default), so the transport waits that long plus its margin.
    expect(calls[0].options?.transportTimeoutMs).toBe(1_800_000 + 10_000);
  });

  test("subagent wait:true transport budget follows a configured worker_wait_max_ms", async () => {
    _resetSubagentCacheForTest();
    const { calls, tool: bash } = createSubagentHarness(
      () => ({ success: true, status: "completed", task_id: "bash-sub-cfg", exit_code: 0 }),
      undefined,
      { bash: { worker_wait_max_ms: 7_200_000 } } as PluginContext["config"],
    );
    await bash.execute(
      { command: "long-build", wait: true },
      createMockSdkContext({ sessionID: "ses_subagent_wait_cfg" }),
    );
    expect(calls[0].options?.transportTimeoutMs).toBe(7_200_000 + 10_000);
  });

  test("subagent wait:true with an explicit timeout keeps it and a matching transport budget", async () => {
    _resetSubagentCacheForTest();
    const { calls, tool: bash } = createSubagentHarness(() => ({
      success: true,
      status: "completed",
      task_id: "bash-sub-wait-timeout",
      exit_code: 0,
      output: "built",
    }));
    await bash.execute(
      { command: "long-build", wait: true, timeout: 45_000 },
      createMockSdkContext({ sessionID: "ses_subagent_wait_timeout" }),
    );
    expect(calls[0].params.timeout).toBe(45_000);
    expect(calls[0].options?.transportTimeoutMs).toBe(45_000 + 10_000);
  });

  test("primary wait:true without a timeout keeps the 30-minute budget", async () => {
    _resetSubagentCacheForTest();
    const { calls, tool: bash } = createHarness(() => ({
      success: true,
      status: "completed",
      task_id: "bash-primary-wait",
      exit_code: 0,
      output: "built",
    }));
    await bash.execute(
      { command: "long-build", wait: true },
      createMockSdkContext({ sessionID: "ses_primary_wait" }),
    );
    expect(calls[0].params.worker_session).toBeUndefined();
    expect(calls[0].options?.transportTimeoutMs).toBe(30 * 60 * 1000 + 10_000);
  });

  test("primary blocking transport budget follows worker_wait_max_ms without extending command timeout", async () => {
    _resetSubagentCacheForTest();
    for (const [timeout, expected] of [
      [undefined, 7_200_000],
      [9_000_000, 7_200_000],
      [45_000, 45_000],
    ] as const) {
      const { calls, tool: bash } = createHarness(
        () => ({ success: true, status: "completed", exit_code: 0 }),
        undefined,
        false,
        { bash: { worker_wait_max_ms: 7_200_000 } } as PluginContext["config"],
      );
      await bash.execute(
        { command: "long-build", wait: true, timeout },
        createMockSdkContext({ sessionID: "ses_primary_cap" }),
      );
      expect(calls[0].params.timeout).toBe(timeout);
      expect(calls[0].options?.transportTimeoutMs).toBe(expected + 10_000);
    }
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
            // The engine's reply to a worker says the task won't wake it.
            output:
              "Background task started: bash-sub-bg. It won't wake you when it finishes, so wait for it before you report a result.",
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
    // The bash_watch call suggested to the subagent must omit timeoutMs: a
    // subagent's watch without one waits up to the worker wait limit, the
    // longest it may, and any value only makes it return sooner.
    expect(result as string).toContain('bash_watch({ taskId: "bash-sub-bg" })');
    expect(result as string).toContain(
      "without a timeout it waits up to the worker wait limit (`bash.worker_wait_max_ms`, 30 minutes by default), then reports it is still running; watch again to keep waiting",
    );
    expect(result as string).not.toContain("waits until the command finishes");
    // A subagent is never woken after its turn ends, so nothing may promise
    // it a completion reminder or tell it to end its turn.
    expect(result as string).not.toContain("completion reminder will be delivered");
    expect(result as string).not.toContain("end the turn");
    expect(result as string).not.toContain("timeoutMs: 60000");
    expect(calls.find((c) => c.command === "bash")?.params.background).toBe(true);
    expect(calls.find((c) => c.command === "bash")?.params.notify_on_completion).toBe(true);
    // The engine can only word its reply for a worker if the request carries worker_session.
    expect(calls.find((c) => c.command === "bash")?.params.worker_session).toBe(true);
  });

  test("subagent auto-promotion with subagent_background true includes guidance", async () => {
    _resetSubagentCacheForTest();
    const { calls, tool: bash } = createSubagentHarness(
      () => ({
        success: true,
        status: "running",
        task_id: "bash-sub-promote",
        output: `Foreground bash didn't finish within 0s and was promoted to background: bash-sub-promote. It won't wake you when it finishes, so wait for it before you report a result; use bash_status({ taskId: "bash-sub-promote" }) to inspect output or bash_kill({ taskId: "bash-sub-promote" }) to terminate.`,
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
    expect(result as string).toContain('bash_watch({ taskId: "bash-sub-promote" })');
    expect(result as string).not.toContain("completion reminder will be delivered");
    expect(result as string).toContain('use bash_status({ taskId: "bash-sub-promote" })');
    expect(calls[0].params.worker_session).toBe(true);
    expect(result as string).not.toContain("timeoutMs: 60000");
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
    // One description serves both roles, so it states both defaults.
    expect(desc).toContain("default to 30s in a main session");
    expect(desc).toContain(
      "in a delegated session a sync wait without a timeout waits up to the worker wait limit (`bash.worker_wait_max_ms`, 30 minutes by default), then reports it is still running; watch again to keep waiting",
    );
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
    expect(jsonSchema.properties?.timeoutMs?.description).toContain(
      "In a main session: default 30000",
    );
    expect(jsonSchema.properties?.timeoutMs?.description).toContain(
      "In a delegated session: omit it to wait up to the worker wait limit",
    );
    expect(jsonSchema.properties?.timeoutMs?.description).not.toContain(
      "the configured maximum for delegated sessions",
    );
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
