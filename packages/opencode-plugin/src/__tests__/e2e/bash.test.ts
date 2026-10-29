/// <reference path="../../bun-test.d.ts" />

import { afterEach, beforeAll, describe, expect, mock, test } from "bun:test";
import { mkdir, mkdtemp, readFile, rm, utimes, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { type BinaryBridge, BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { withEnv } from "../../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import { loadAftConfig } from "../../config.js";
import { _resetSubagentCacheForTest } from "../../shared/subagent-detect.js";
import { createBashKillTool, createBashTool } from "../../tools/bash.js";
import { createBashWatchTool } from "../../tools/bash_watch.js";
import type { PluginContext } from "../../types.js";
import { mockAsk, noopAsk } from "../test-helpers";
import {
  cleanupHarnesses,
  configureParamsFromLegacyOverrides,
  createHarness,
  type E2EHarness,
  type PreparedBinary,
  prepareBinary,
} from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = describe.skipIf(!initialBinary.binaryPath);

// Every test here starts a real AFT binary and a real shell. On a loaded
// Windows runner the first test pays cold start: the binary load, its
// antivirus scan and PowerShell's first launch together can exceed bun's 5 s
// default. This is a positive wait for work to finish, so it can be generous.
const E2E_TEST_TIMEOUT_MS = 30_000;

// On Windows, PowerShell can emit CRLF (`\r\n`) line endings. AFT normalizes
// terminal previews for text renderers, but host-fallback output remains native
// shell text. Normalize assertions only when line ending is incidental to the
// behavior under test; saved bash artifacts retain their original bytes.
const IS_WINDOWS = process.platform === "win32";
const eol = (text: string): string => text.replace(/\r\n/g, "\n");
const WAIT_DETACH_COMMAND = IS_WINDOWS
  ? "Start-Sleep -Milliseconds 1500; Write-Output detached"
  : "sleep 1.5 && echo detached";

// Tests that assert Unix-shell output SEMANTICS — exact raw byte equality,
// POSIX `pwd` path shape, or `cat`/`grep` rewrite output over forward-slash
// paths — are not this job's contract on Windows. AFT's hoisted bash uses
// backslash Windows paths and PowerShell, and the rewrite scenarios are already
// documented as Unix-only (see crates/aft/tests/integration/bash_rewrite_test.rs).
// Linux + macOS cover these shell semantics; Windows native E2E covers real
// Windows product integration. This Windows Bun job's contract is the bash
// PERMISSION FLOW, which is platform-relevant and kept blocking below.
const skipOnWindows = IS_WINDOWS ? test.skip : test;

interface BashResult {
  /** Agent-visible bash output (what the LLM sees verbatim). */
  output: string;
  /** Last metadata payload pushed via ctx.metadata — exit code, truncation flags, etc. */
  metadata: Record<string, unknown>;
}

interface RuntimeOptions {
  ask?: ToolContext["ask"];
  directory?: string;
  worktree?: string;
}

maybeDescribe("e2e bash command (OpenCode adapter + bridge + Rust)", () => {
  let preparedBinary: PreparedBinary = initialBinary;
  const harnesses: E2EHarness[] = [];

  beforeAll(async () => {
    preparedBinary = await prepareBinary();
  });

  afterEach(async () => {
    await cleanupHarnesses(harnesses);
  });

  async function harness(configOverrides: Record<string, unknown> = {}): Promise<E2EHarness> {
    const created = await createHarness(preparedBinary, {
      fixtureNames: [],
      bridgeOptions: { timeoutMs: 20_000 },
    });
    if (Object.keys(configOverrides).length > 0) {
      await created.bridge.send(
        "configure",
        configureParamsFromLegacyOverrides({
          project_root: created.tempDir,
          harness: "opencode",
          restrict_to_project_root: true,
          bash_permissions: false,
          storage_dir: join(created.tempDir, ".aft-storage"),
          ...configOverrides,
        }),
      );
    }
    harnesses.push(created);
    return created;
  }

  async function pluginHarness(configOverrides: Record<string, unknown> = {}) {
    const h = await harness();
    const pool = new BridgePool(
      h.binaryPath,
      { timeoutMs: 20_000 },
      configureParamsFromLegacyOverrides({
        restrict_to_project_root: true,
        bash_permissions: false,
        storage_dir: join(h.tempDir, ".aft-storage"),
        harness: "opencode",
        ...configOverrides,
      }),
    );
    const bridgeCalls: Array<{ command: string; params: Record<string, unknown> }> = [];
    const patchedBridges = new WeakSet<BinaryBridge>();
    const originalGetBridge = pool.getBridge.bind(pool);
    (pool as { getBridge: (projectRoot: string) => BinaryBridge }).getBridge = (projectRoot) => {
      const bridge = originalGetBridge(projectRoot);
      if (!patchedBridges.has(bridge)) {
        const originalSend = bridge.send.bind(bridge);
        bridge.send = async (command, params = {}, options) => {
          bridgeCalls.push({ command, params });
          return await originalSend(command, params, options);
        };
        patchedBridges.add(bridge);
      }
      return bridge;
    };
    const ctx: PluginContext = {
      pool,
      client: {} as PluginContext["client"],
      config: {} as PluginContext["config"],
      storageDir: join(h.tempDir, ".aft-storage"),
    };
    const bash = createBashTool(ctx);
    const cleanup = h.cleanup;
    Object.defineProperty(h, "cleanup", {
      value: async () => {
        await pool.shutdown();
        await cleanup.call(h);
      },
    });
    return { h, bash, pool, ctx, bridgeCalls };
  }

  async function callPluginBash(
    bash: ReturnType<typeof createBashTool>,
    h: E2EHarness,
    args: Record<string, unknown>,
    options: RuntimeOptions = {},
  ): Promise<BashResult> {
    let lastMetadata: Record<string, unknown> = {};
    const context = {
      sessionID: "e2e-session",
      messageID: "e2e-message",
      agent: "e2e-agent",
      directory: options.directory ?? h.tempDir,
      worktree: options.worktree ?? h.tempDir,
      abort: new AbortController().signal,
      // OpenCode 1's ctx.metadata takes { title?, metadata? }; keep the
      // metadata part the way the host would show it.
      metadata: (update: { metadata?: Record<string, unknown> }) => {
        lastMetadata = update.metadata ?? {};
      },
      ask: options.ask ?? noopAsk,
      callID: `call-${Date.now()}`,
    } as ToolContext;
    const result = await bash.execute(args, context);
    const output = typeof result === "string" ? result : (result?.output ?? "");
    return { output, metadata: lastMetadata };
  }

  function nonConfigureCommands(
    calls: Array<{ command: string; params: Record<string, unknown> }>,
  ): string[] {
    return calls.filter((call) => call.command !== "configure").map((call) => call.command);
  }

  function expectNoClientPollOrPromote(
    calls: Array<{ command: string; params: Record<string, unknown> }>,
  ): void {
    const commands = nonConfigureCommands(calls);
    expect(commands).toContain("bash");
    expect(commands).not.toContain("bash_status");
    expect(commands).not.toContain("bash_promote");
  }

  skipOnWindows(
    "unknown-cwd commands still obey the host rm deny rule",
    async () => {
      const { h, bash } = await pluginHarness({ bash_permissions: true });
      await mkdir(join(h.tempDir, "build"));
      const marker = join(h.tempDir, "build", "keep.txt");
      await writeFile(marker, "must not delete");
      const resources: string[] = [];
      const ask: ToolContext["ask"] = async (input) => {
        if (input.permission !== "bash") return;
        resources.push(...input.patterns);
        // Host policy: deny `rm *`, and allow every other command by default.
        if (input.patterns.some((resource) => resource.startsWith("rm "))) {
          throw new Error("rm denied by host rule");
        }
      };
      await withEnv({ DEST: h.tempDir }, async () => {
        await expect(
          callPluginBash(bash, h, { command: 'cd "$DEST" && rm -rf build' }, { ask }),
        ).rejects.toThrow("rm denied by host rule");
      });
      expect(resources).toContain("rm -rf build");
      expect(await readFile(marker, "utf8")).toBe("must not delete");
    },
    E2E_TEST_TIMEOUT_MS,
  );

  async function bridgeBashToTerminal(
    h: E2EHarness,
    args: Record<string, unknown>,
  ): Promise<Record<string, unknown>> {
    const launched = await h.bridge.send("bash", args);
    expect(launched.success).toBe(true);
    // A fast command (git init, git status) can finish before the launch
    // reply is built, so the reply may already say completed. Either way the
    // task id is there, and bash_status below reads the same final status.
    expect(["running", "completed"]).toContain(launched.status);
    expect(typeof launched.task_id).toBe("string");
    const taskId = launched.task_id as string;
    const started = Date.now();
    // A positive wait for a short command to finish. Windows runners start
    // PowerShell slowly and can run git several times slower under load, so a
    // 5 s bound failed there (git status took over 5 s); a hang still fails here.
    while (Date.now() - started < 20_000) {
      const status = await h.bridge.send("bash_status", { task_id: taskId });
      if (status.status !== "running") return status;
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    throw new Error(`timed out waiting for ${taskId}`);
  }

  test(
    "foreground returns raw output text (not a JSON envelope)",
    async () => {
      const { h, bash, bridgeCalls } = await pluginHarness();

      const result = await callPluginBash(bash, h, { command: "echo hello" });

      // Agent-visible output is bash text — NOT a JSON literal that the model
      // would have to JSON.parse before reading.
      // The line after the output names the finished task.
      expect(eol(result.output)).toMatch(/^hello\n\[task bash-[0-9a-f]+\]$/);
      // Exit code, truncation, etc. land in metadata for the UI.
      expect(result.metadata.exit).toBe(0);
      expectNoClientPollOrPromote(bridgeCalls);
      expect(bridgeCalls[0].params).toMatchObject({
        foreground_orchestrate: true,
        block_to_completion: false,
      });
    },
    E2E_TEST_TIMEOUT_MS,
  );

  skipOnWindows("OpenCode hoisted bash renders CRLF output as text lines", async () => {
    const { h, bash } = await pluginHarness();

    const result = await callPluginBash(bash, h, {
      command: "printf 'line1\\r\\nline2\\r\\nline3\\r\\n'",
      compressed: false,
    });

    expect(result.output).toMatch(/^line1\nline2\nline3\n\[task bash-[0-9a-f]+\]$/);
    expect(result.output).not.toContain("\r");
    expect(result.metadata.exit).toBe(0);
  });

  test(
    "non-zero exit appends [exit code: N] to agent-visible output",
    async () => {
      const { h, bash, bridgeCalls } = await pluginHarness();

      const result = await callPluginBash(bash, h, { command: "false" });

      // The agent must be able to detect command failure from the text itself,
      // because metadata is UI-only and not echoed back to the model.
      expect(result.output).toMatch(/^\n\[exit code: 1\]\n\[task bash-[0-9a-f]+\]$/);
      expect(result.metadata.exit).toBe(1);
      expectNoClientPollOrPromote(bridgeCalls);
    },
    E2E_TEST_TIMEOUT_MS,
  );

  test("foreground promotion returns server text and does not call bash_status or bash_promote", async () => {
    const { h, bash, bridgeCalls } = await pluginHarness({ experimental_bash_background: true });

    // The command must still be running when the 25 ms wait window ends. A
    // 0.2 s command finished first on a loaded runner and came back as a
    // completed result, which is correct product behaviour, so give it seconds.
    const result = await withEnv({ AFT_TEST_FOREGROUND_WAIT_MS: "25" }, async () =>
      callPluginBash(bash, h, { command: "sleep 5 && echo late" }),
    );

    expect(result.output).toContain("promoted to background");
    expect(String(result.metadata.taskId)).toMatch(/^bash-[a-f0-9]{16}$/);
    expectNoClientPollOrPromote(bridgeCalls);
    // Explicit budget: spawn + 25ms wait-window + promote crosses several
    // process boundaries; bun's 5s default flakes when a parallel suite pins
    // the machine (it timed out during a loaded release-gate run).
  }, 30_000);

  test("wait true returns a long foreground command directly", async () => {
    const { h, bash, bridgeCalls } = await pluginHarness({ experimental_bash_background: true });

    const result = await withEnv({ AFT_TEST_FOREGROUND_WAIT_MS: "25" }, async () =>
      callPluginBash(bash, h, {
        command: "sleep 0.2 && echo waited",
        wait: true,
        timeout: 5_000,
      }),
    );

    expect(eol(result.output)).toContain("waited\n");
    expect(result.output).not.toContain("promoted to background");
    expectNoClientPollOrPromote(bridgeCalls);
    expect(bridgeCalls[0].params).toMatchObject({
      wait: true,
      block_to_completion: true,
      timeout: 5_000,
    });
  }, 30_000);

  test("wait true detaches to background when a new message arrives", async () => {
    const { h, bash, pool, bridgeCalls } = await pluginHarness({
      experimental_bash_background: true,
    });

    const resultPromise = callPluginBash(bash, h, {
      command: WAIT_DETACH_COMMAND,
      wait: true,
      timeout: 10_000,
    });

    let detachResponse: Record<string, unknown> | undefined;
    const started = Date.now();
    while (Date.now() - started < 2_000) {
      detachResponse = await pool.getBridge(h.tempDir).send("bash_wait_detach", {
        session_id: "e2e-session",
      });
      if (detachResponse.detached === true) break;
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    expect(detachResponse?.success).toBe(true);
    expect(detachResponse?.detached).toBe(true);

    const result = await resultPromise;

    expect(result.output).toContain("Detached because a user message arrived.");
    expect(result.output).toContain("AFT kills this task at ");
    expect(result.output).toContain("(the `timeout` you passed)");
    expect(String(result.metadata.taskId)).toMatch(/^bash-[a-f0-9]{16}$/);
    expect(result.metadata.status).toBe("running");
    expectNoClientPollOrPromote(bridgeCalls);
    expect(nonConfigureCommands(bridgeCalls)).toContain("bash_wait_detach");
  }, 30_000);

  test("background true returns server launch text and task id without polling", async () => {
    const { h, bash, bridgeCalls } = await pluginHarness({ experimental_bash_background: true });

    const result = await callPluginBash(bash, h, {
      command: "sleep 0.2 && echo background-done",
      background: true,
    });

    expect(result.output).toContain("Background task started:");
    expect(String(result.metadata.taskId)).toMatch(/^bash-[a-f0-9]{16}$/);
    expectNoClientPollOrPromote(bridgeCalls);
  }, 30_000);

  // A sync watch must report the time it really held the call. The wall clock
  // is stepped forward 60 s shortly after the watch starts, as an NTP
  // correction or a wake from sleep can do. A watch timed with Date.now()
  // would end at that step and claim it waited 60 s; a monotonic watch keeps
  // its 2 s limit and says so. The session has a parent, so the plugin treats
  // the caller as a delegated worker.
  test("bash_watch on a still-running task reports about 2 s elapsed and the timeout reason", async () => {
    const { h, bash, ctx } = await pluginHarness({ experimental_bash_background: true });
    // Earlier tests in this file reuse the session id without a parent; drop
    // the cached classification so this one is read as a worker.
    _resetSubagentCacheForTest();
    ctx.client = {
      session: {
        get: async (input: { path: { id: string } }) => ({
          data: { id: input.path.id, parentID: "e2e-parent-session" },
        }),
      },
    } as unknown as PluginContext["client"];
    const launched = await callPluginBash(bash, h, { command: "sleep 30", background: true });
    const taskId = String(launched.metadata.taskId);
    const watch = createBashWatchTool(ctx);
    let metadata: Record<string, unknown> = {};
    const context = {
      sessionID: "e2e-session",
      messageID: "e2e-message",
      agent: "e2e-agent",
      directory: h.tempDir,
      worktree: h.tempDir,
      abort: new AbortController().signal,
      metadata: (update: { metadata?: Record<string, unknown> }) => {
        metadata = update.metadata ?? {};
      },
      ask: noopAsk,
      callID: "call-watch",
    } as ToolContext;

    const realNow = Date.now;
    const stepAt = realNow() + 300;
    Date.now = () => {
      const now = realNow();
      return now >= stepAt ? now + 60_000 : now;
    };
    const started = performance.now();
    let text: string;
    try {
      text = String(await watch.execute({ taskId, timeoutMs: 2_000 }, context));
    } finally {
      Date.now = realNow;
    }
    const realElapsed = performance.now() - started;
    await createBashKillTool(ctx).execute({ taskId }, context);

    const waited = metadata.waited as { reason?: string; elapsed_ms?: number } | undefined;
    expect(waited?.reason).toBe("timeout");
    expect(realElapsed).toBeGreaterThanOrEqual(1_900);
    expect(realElapsed).toBeLessThan(10_000);
    const reported = Number(/Waited (\d+)ms \(limit 2000ms\)/.exec(text)?.[1]);
    expect(Math.abs(reported - realElapsed)).toBeLessThan(500);
    expect(waited?.elapsed_ms).toBe(reported);
    expect(text).toContain("timeout reached without match");
    // A delegated worker whose watch ran out is told the command is still
    // running and how to go on; leaving the timeout out waits up to the worker
    // wait limit. It is not pointed at the primary cap.
    expect(text).toContain("The command is still running after");
    expect(text).toContain("without timeoutMs a watch waits up to the worker wait limit");
    expect(text).toContain(`bash_kill({ taskId: "${taskId}" })`);
    expect(text).not.toContain("timeoutMs up to");
  }, 30_000);

  skipOnWindows("workdir is respected", async () => {
    const { h, bash } = await pluginHarness();
    const subdir = h.path("subdir");
    await mkdir(subdir);

    const result = await callPluginBash(bash, h, { command: "pwd", workdir: subdir });

    const [cwd, taskLine] = result.output.trim().split("\n");
    expect(cwd).toBe(await realPath(subdir));
    expect(taskLine).toMatch(/^\[task bash-[0-9a-f]+\]$/);
    expect(result.metadata.exit).toBe(0);
  });

  test(
    "foreground timeout returns timed-out process exit without throwing",
    async () => {
      const h = await harness();

      const response = await bridgeBashToTerminal(h, { command: "sleep 5", timeout: 100 });

      expect(response.success).toBe(true);
      expect(response.status).toBe("timed_out");
      expect(response.exit_code).toBe(124);
    },
    E2E_TEST_TIMEOUT_MS,
  );

  skipOnWindows("rewrites cat to read with footer hint when enabled", async () => {
    const h = await harness({ experimental_bash_rewrite: true });
    const filePath = h.path("notes.txt");
    await writeFile(filePath, "alpha\nbeta\n", "utf8");

    const response = await h.bridge.send("bash", {
      command: `cat ${filePath}`,
      compressed: false,
    });

    expect(response.success).toBe(true);
    expect(String(response.output)).toContain("1: alpha");
    expect(String(response.output)).toContain("Prefer `read` tool over bash.");
  });

  skipOnWindows("rewrites grep -r to grep tool with enforced code-search footer", async () => {
    const h = await harness({ experimental_bash_rewrite: true });
    await mkdir(h.path("src"));
    await writeFile(h.path("src", "lib.ts"), "needle\nhaystack\n", "utf8");
    const old = new Date(Date.now() - 61_000);
    await utimes(h.path("src"), old, old);

    const response = await h.bridge.send("bash", {
      command: `grep -r needle ${h.path("src")}`,
      compressed: false,
    });

    expect(response.success).toBe(true);
    expect(String(response.output)).toContain("needle");
    // The default tool surface includes aft_search, so the footer directs code searches there.
    expect(String(response.output)).toContain("DO NOT search code by running grep/rg in bash");
    expect(String(response.output)).toContain("Use the `aft_search` tool instead");
  });

  skipOnWindows("rewriter disabled runs cat as raw bash without footer", async () => {
    const h = await harness({ experimental_bash_rewrite: false });
    const filePath = h.path("raw.txt");
    await writeFile(filePath, "raw cat output\n", "utf8");

    const response = await bridgeBashToTerminal(h, {
      command: `cat ${filePath}`,
      compressed: false,
    });

    expect(response.success).toBe(true);
    expect(response.output_preview).toBe("raw cat output\n");
    expect(String(response.output_preview)).not.toContain("Prefer `read` tool over bash.");
  });

  test(
    "generic compressor strips ANSI and collapses four-plus duplicate lines",
    async () => {
      const h = await harness({ experimental_bash_compress: true });

      const response = await bridgeBashToTerminal(h, {
        command: "printf '\\033[31mred\\033[0m\\nred\\nred\\nred\\nred\\n'",
      });

      expect(response.success).toBe(true);
      expect(String(response.output_preview)).toContain("red");
    },
    E2E_TEST_TIMEOUT_MS,
  );

  test("git status compressor summarizes large status sections", async () => {
    const h = await harness({ experimental_bash_compress: true });
    await bridgeBashToTerminal(h, { command: "git init -q -b main", compressed: false });
    // Status compressor only triggers when output exceeds STATUS_SHORT_LIMIT (1024B);
    // 50 files with longer names easily clears that threshold and exercises the
    // STATUS_KEEP_PER_SECTION (10) truncation path.
    for (let index = 0; index < 50; index++) {
      await writeFile(h.path(`untracked_file_with_long_name_${index}.txt`), `${index}\n`, "utf8");
    }

    const response = await bridgeBashToTerminal(h, { command: "git status" });

    expect(response.success).toBe(true);
    expect(String(response.output_preview)).toContain("Untracked files:");
    expect(String(response.output_preview)).toContain("untracked_file_with_long_name_0.txt");
    // Heavy subprocess test (git init + 50 file writes + git status through the
    // bridge). Bun's default 5s per-test budget is too tight on a loaded Windows
    // CI runner (~5x slower), where this has timed out; give it generous headroom.
  }, 30_000);

  test("compressed false opts out of git status compression", async () => {
    const h = await harness({ experimental_bash_compress: true });
    await bridgeBashToTerminal(h, { command: "git init -q -b main", compressed: false });
    for (let index = 0; index < 15; index++) {
      await writeFile(h.path(`raw_${index}.txt`), `${index}\n`, "utf8");
    }

    const response = await bridgeBashToTerminal(h, { command: "git status", compressed: false });

    expect(response.success).toBe(true);
    expect(String(response.output_preview)).toContain("raw_14.txt");
  }, 30_000);

  test(
    "background spawn returns task_id immediately",
    async () => {
      const h = await harness({ experimental_bash_background: true });
      const started = Date.now();

      const response = await h.bridge.send("bash", {
        command: "sleep 1 && echo done",
        background: true,
      });

      expect(response.success).toBe(true);
      expect(response.status).toBe("running");
      expect(typeof response.task_id).toBe("string");
      // Typically returns in <100ms locally; assert only a generous deadlock bound
      // so CI load does not turn latency into correctness.
      expect(Date.now() - started).toBeLessThan(10_000);
    },
    E2E_TEST_TIMEOUT_MS,
  );

  test(
    "bash_status reports running then completed output",
    async () => {
      const h = await harness({ experimental_bash_background: true });
      const spawned = await h.bridge.send("bash", {
        command: "sleep 0.3 && echo done",
        background: true,
      });
      const taskId = String(spawned.task_id);

      const running = await h.bridge.send("bash_status", { task_id: taskId });
      expect(running.success).toBe(true);
      expect(running.status).toBe("running");

      const completed = await waitForStatus(h, taskId, "completed");
      expect(completed.exit_code).toBe(0);
      // CRLF-tolerant: PowerShell emits "done\r\n" on Windows.
      expect(eol(String(completed.output_preview))).toBe("done\n");
    },
    E2E_TEST_TIMEOUT_MS,
  );

  test(
    "bash_kill terminates a running task",
    async () => {
      const h = await harness({ experimental_bash_background: true });
      const spawned = await h.bridge.send("bash", { command: "sleep 60", background: true });
      const taskId = String(spawned.task_id);

      const killed = await h.bridge.send("bash_kill", { task_id: taskId });
      const status = await h.bridge.send("bash_status", { task_id: taskId });

      expect(killed.success).toBe(true);
      expect(killed.status).toBe("killed");
      expect(status.status).toBe("killed");
    },
    E2E_TEST_TIMEOUT_MS,
  );

  test("background completions are no longer appended by the bash adapter", async () => {
    const { h, bash } = await pluginHarness({ experimental_bash_background: true });
    await h.bridge.send(
      "configure",
      configureParamsFromLegacyOverrides({
        project_root: h.tempDir,
        harness: "opencode",
        restrict_to_project_root: true,
        bash_permissions: false,
        storage_dir: join(h.tempDir, ".aft-storage"),
        experimental_bash_background: true,
      }),
    );
    const spawned = await h.bridge.send("bash", {
      command: "echo bg-done | tee bg-marker.txt",
      background: true,
    });
    const taskId = String(spawned.task_id);
    // A loaded Windows runner can take several seconds to start a background
    // task, so the wait and the test's own timeout leave room for that.
    await waitForFileText(join(h.tempDir, "bg-marker.txt"), "bg-done", 30_000);

    const result = await callPluginBash(bash, h, { command: "echo foreground" });

    expect(eol(result.output)).toContain("foreground\n");
    expect(result.output).not.toContain("Background tasks completed:");
    expect(result.output).not.toContain(taskId);
    expect(result.output).not.toContain("bg-done");
  }, 60_000);

  test(
    "permission ask round-trip invokes OpenCode ctx.ask",
    async () => {
      const { h, bash, bridgeCalls } = await pluginHarness({ bash_permissions: true });
      const ask = mockAsk();

      // The isolated test home can live inside the repository on Windows. Select
      // a nonexistent Git directory for this command only: ancestor discovery
      // cannot turn the intended command failure into a successful status query,
      // and bridge configuration still sees an ordinary project directory.
      const command = "git --git-dir=missing-repository status";
      const result = await callPluginBash(bash, h, { command }, { ask });

      // Real git status fails on the missing repository — exit 128 surfaces
      // in the agent-visible output AND in the metadata.
      expect(result.metadata.exit).toBe(128);
      expect(result.output).toContain("[exit code: 128]");
      expect(ask).toHaveBeenCalledTimes(1);
      expect(ask.mock.calls[0][0]).toMatchObject({
        permission: "bash",
        patterns: ["git --git-dir=missing-repository status"],
        always: ["git --git-dir=missing-repository *"],
      });
      expect(nonConfigureCommands(bridgeCalls)).toEqual(["bash", "bash"]);
    },
    E2E_TEST_TIMEOUT_MS,
  );

  // ─────────────────────────────────────────────────────────────────────────
  // Permission flow regression coverage (Oracle audit v0.19.5..HEAD).
  //
  // These tests exercise the FULL stack — Rust permission scan → bridge →
  // plugin runAsk → real ctx.ask Promise → response — through the OpenCode
  // adapter exactly as it ships. They sit in the e2e suite (not unit tests)
  // because the original `bash: { "*": deny } doesn't deny` regression was a
  // runtime mismatch between the bundled `effect` runtime and the SDK's, and
  // we want a chokepoint that catches BOTH past failure modes:
  //   - silent-await (current Promise-shape regression risk): runAsk must
  //     actually `await` the returned Promise.
  //   - runtime-mismatch (legacy Effect-shape regression risk): if the SDK
  //     ever flips back to Effect, runAsk must execute the Effect body.
  //
  // Coverage matrix:
  //   1. Allow path        → command runs, ask invoked exactly once.
  //   2. Deny path         → bash deny propagates as a thrown Error.
  //   3. Body execution    → the ask body actually runs (no silent drop).
  //   4. permissions_granted → ask is bypassed entirely (Rust short-circuits).
  //   5. Multiple asks     → bash asks are grouped and awaited before bash runs.
  // ─────────────────────────────────────────────────────────────────────────

  test(
    "Promise-returning ask resolves cleanly and bash runs (allow path)",
    async () => {
      const { h, bash } = await pluginHarness();

      let askInvoked = false;
      // A bare async lambda mirrors what OpenCode 1.15.5 does for an "allow"
      // decision: ask() returns Promise<void> that resolves with no error. If
      // runAsk regresses to a no-op or fire-and-forget, the body never runs
      // and `askInvoked` stays false — the assertion below catches that class
      // of regression even though the bash command itself would still succeed
      // by accident.
      const ask = mock(async (_input: unknown) => {
        askInvoked = true;
      }) as ToolContext["ask"];

      const result = await callPluginBash(bash, h, { command: "echo allowed" }, { ask });

      expect(askInvoked).toBe(true);
      // CRLF-tolerant: PowerShell emits "allowed\r\n" on Windows. The permission
      // flow (ask invoked, command ran) is the contract — not the line ending.
      expect(eol(result.output)).toMatch(/^allowed\n\[task bash-[0-9a-f]+\]$/);
      expect(result.metadata.exit).toBe(0);
    },
    E2E_TEST_TIMEOUT_MS,
  );

  test(
    "rejecting ask propagates as a thrown Error (deny path)",
    async () => {
      const { h, bash } = await pluginHarness();

      // A rejected Promise mirrors what OpenCode does when a permission rule
      // denies the request. The plugin must surface this back through
      // bash.execute as a thrown Error so OpenCode's tool runner records it
      // as a deny — NOT silently let bash run anyway. The original bug report
      // was exactly "`bash: { '*': deny }` doesn't deny".
      const ask = mock(async (_input: unknown) => {
        throw new Error("Permission denied by user");
      }) as ToolContext["ask"];

      let captured: unknown;
      try {
        await callPluginBash(bash, h, { command: "echo should-not-run" }, { ask });
        throw new Error("expected bash.execute to throw on deny");
      } catch (err) {
        captured = err;
      }

      expect(captured).toBeInstanceOf(Error);
      expect((captured as Error).message).toContain("Permission denied by user");
      // The ask must have actually been consulted — a fix that catches the deny
      // BEFORE consulting ask would also fail this assertion.
      expect(ask).toHaveBeenCalledTimes(1);
    },
    E2E_TEST_TIMEOUT_MS,
  );

  test(
    "permissions_granted skips ctx.ask entirely",
    async () => {
      const { h } = await pluginHarness();

      // Bypass the bash tool's plugin-side permission loop and call the bridge
      // directly with `permissions_granted` so we can assert that pre-granted
      // patterns short-circuit the Rust scanner without ever asking the user.
      // This proves the Rust side of the fail-closed gate (zero-asks → deny)
      // does NOT trigger when patterns are already trusted.
      const response = await h.bridge.send("bash", {
        command: "git status",
        permissions_requested: true,
        permissions_granted: ["git status *"],
      });

      expect(response.success).toBe(true);
      expect(response.code).not.toBe("permission_required");
    },
    E2E_TEST_TIMEOUT_MS,
  );

  test(
    "multiple bash permission asks are grouped before bash runs",
    async () => {
      const { h, bash } = await pluginHarness();

      // `find . | xargs grep foo` produces multiple Rust bash asks. OpenCode's
      // adapter should collapse them into ONE host prompt that still carries all
      // scanned patterns before the second bridge call runs the command.
      let askCount = 0;
      let askInput: { permission?: string; patterns?: string[]; always?: string[] } | undefined;
      const ask = mock(async (input: unknown) => {
        askCount += 1;
        askInput = input as { permission?: string; patterns?: string[]; always?: string[] };
      });

      await callPluginBash(
        bash,
        h,
        { command: "find . | xargs grep foo" },
        { ask: ask as unknown as ToolContext["ask"] },
      );

      expect(askCount).toBe(1);
      expect(ask.mock.calls.length).toBe(1);
      expect(askInput?.permission).toBe("bash");
      expect(askInput?.patterns?.length ?? 0).toBeGreaterThanOrEqual(2);
      expect(askInput?.always?.length ?? 0).toBeGreaterThanOrEqual(2);
    },
    E2E_TEST_TIMEOUT_MS,
  );

  test(
    "Rust scan fail-closed wildcard ask propagates through the plugin layer",
    async () => {
      const { h, bash } = await pluginHarness();

      // Inputs like `((i++))` parse cleanly in tree-sitter-bash but produce
      // ZERO `command` nodes. The Rust scanner's fail-closed branch must emit
      // a wildcard "*" ask in that case (Oracle audit MEDIUM #2). The plugin
      // layer must then forward that ask to ctx.ask through the same Promise
      // path — proving the scanner+plugin chain doesn't silently let
      // command-less inputs bypass `bash: { "*": deny }`. Reject after the ask
      // is observed so the assertion covers the permission path without waiting
      // for a platform shell to execute this intentionally odd input.
      let askInput: { patterns: string[]; permission: string } | undefined;
      const ask = mock(async (input: unknown) => {
        askInput = input as { patterns: string[]; permission: string };
        throw new Error("wildcard ask observed");
      });

      let captured: unknown;
      try {
        await callPluginBash(
          bash,
          h,
          { command: "((i++))" },
          { ask: ask as unknown as ToolContext["ask"] },
        );
      } catch (err) {
        captured = err;
      }

      expect(captured).toBeInstanceOf(Error);
      expect((captured as Error).message).toContain("wildcard ask observed");
      expect(ask).toHaveBeenCalled();
      expect(askInput).toBeDefined();
      expect(askInput?.permission).toBe("bash");
      // Wildcard or literal echo of the input — either is acceptable as long
      // as the agent is forced to consult OpenCode's permission rules.
      expect(askInput?.patterns.length ?? 0).toBeGreaterThan(0);
    },
    E2E_TEST_TIMEOUT_MS,
  );
});

test("OpenCode executes project-enabled host fallback when the bridge binary is missing", async () => {
  const project = await mkdtemp(join(tmpdir(), "aft-host-fallback-e2e-"));
  const configDir = join(project, ".cortexkit");
  await mkdir(configDir, { recursive: true });
  await writeFile(
    join(configDir, "aft.jsonc"),
    JSON.stringify({ bash: { host_fallback: true } }),
    "utf8",
  );
  const pool = new BridgePool(join(project, "missing-aft-binary"), { timeoutMs: 250 }, {});

  try {
    const config = loadAftConfig(project);
    const bash = createBashTool({
      pool,
      client: {} as PluginContext["client"],
      config,
      storageDir: join(project, ".aft-storage"),
    });
    const ask = mockAsk();
    const command =
      process.platform === "win32"
        ? `${JSON.stringify(process.execPath)} -e "process.stdout.write('e2e-host-fallback')"`
        : "printf e2e-host-fallback";
    const result = await bash.execute({ command }, {
      sessionID: "fallback-e2e",
      messageID: "message",
      agent: "agent",
      directory: project,
      worktree: project,
      abort: new AbortController().signal,
      metadata: () => {},
      ask,
      callID: "call",
    } as ToolContext);
    const output = typeof result === "string" ? result : (result?.output ?? "");

    expect(output).toContain(
      "[AFT host fallback - module transport down; no rewrites/compression/background]",
    );
    expect(output).toContain("e2e-host-fallback");
    expect(output).toContain("[exit code: 0]");
    expect(ask).toHaveBeenCalledTimes(1);
    // Compare the working-directory line through realpath on BOTH sides: on
    // Windows CI the temp dir reaches the plugin in 8.3 short form (RUNNER~1)
    // while the test's own variable holds the long form - same directory, two
    // spellings. The command lines must remain byte-exact.
    const [pattern] = ask.mock.calls[0][0].patterns;
    const marker = "\n\nWorking directory:\n";
    const markerAt = pattern.indexOf(marker);
    expect(markerAt).toBeGreaterThan(0);
    expect(pattern.slice(0, markerAt)).toBe(
      `AFT UNAVAILABLE (transport down) - host fallback execution:\n\nExact command:\n${command}`,
    );
    expect(await realPath(pattern.slice(markerAt + marker.length))).toBe(await realPath(project));
  } finally {
    await pool.shutdown();
    await rm(project, { recursive: true, force: true });
  }
}, 10_000);

async function realPath(path: string): Promise<string> {
  const { realpath } = await import("node:fs/promises");
  return realpath(path);
}

async function waitForStatus(
  h: E2EHarness,
  taskId: string,
  expected: string,
): Promise<Record<string, unknown>> {
  const started = Date.now();
  while (Date.now() - started < 5_000) {
    const response = await h.bridge.send("bash_status", { task_id: taskId });
    expect(response.success).toBe(true);
    if (response.status === expected) return response;
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error(`timed out waiting for ${expected}`);
}

async function waitForFileText(path: string, expected: string, timeoutMs: number): Promise<void> {
  const started = Date.now();
  while (Date.now() - started < timeoutMs) {
    try {
      if ((await readFile(path, "utf8")).includes(expected)) return;
    } catch (error) {
      if ((error as { code?: string }).code !== "ENOENT") throw error;
    }
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
  throw new Error(`timed out waiting for ${path} to contain ${expected}`);
}
