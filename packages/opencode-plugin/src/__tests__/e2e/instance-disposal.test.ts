/// <reference path="../../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { access, mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { setActiveLogger } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { acquireEnv } from "../../../../aft-bridge/src/__tests__/test-utils/env-guard.js";

type RegisteredCleanup = (reason: string) => Promise<void>;
type PluginHooks = {
  tool: Record<
    string,
    { execute(args: Record<string, unknown>, context: ToolContext): Promise<unknown> }
  >;
  dispose(): Promise<void>;
};
type ShutdownHooksModule = typeof import("../../shutdown-hooks.js");

const AFT_BINARY_NAME = process.platform === "win32" ? "aft.exe" : "aft";
const AFT_BINARY = resolve(import.meta.dir, `../../../../../target/debug/${AFT_BINARY_NAME}`);
const maybeDescribe = describe.skipIf(!existsSync(AFT_BINARY));

function cleanupRegistry(): Set<RegisteredCleanup> {
  const state = (
    globalThis as unknown as {
      __aftShutdownHooks__?: { cleanups: Set<RegisteredCleanup> };
    }
  ).__aftShutdownHooks__;
  return state?.cleanups ?? new Set<RegisteredCleanup>();
}

function onlyNewCleanup(before: ReadonlySet<RegisteredCleanup>): RegisteredCleanup {
  const added = [...cleanupRegistry()].filter((cleanup) => !before.has(cleanup));
  expect(added).toHaveLength(1);
  const cleanup = added[0];
  if (!cleanup) throw new Error("plugin factory did not register its shutdown cleanup");
  return cleanup;
}

function shellCommand(marker: string, seconds: number, output: string): string {
  if (process.platform === "win32") {
    const path = marker.replaceAll("'", "''");
    return `Set-Content -Path '${path}' -Value started; Start-Sleep -Seconds ${seconds}; Write-Output ${output}`;
  }
  const path = marker.replaceAll("'", "'\\''");
  return `printf started > '${path}'; sleep ${seconds}; printf ${output}`;
}

function warmCommand(output: string): string {
  return process.platform === "win32" ? `Write-Output ${output}` : `printf ${output}`;
}

async function waitForFile(path: string): Promise<void> {
  const deadline = Date.now() + 5_000;
  while (Date.now() < deadline) {
    try {
      await access(path);
      return;
    } catch {
      await new Promise((resolveWait) => setTimeout(resolveWait, 25));
    }
  }
  throw new Error(`timed out waiting for held command marker: ${path}`);
}

function toolContext(directory: string, sessionID: string): ToolContext {
  return {
    sessionID,
    messageID: `${sessionID}-message`,
    agent: "instance-disposal-test",
    directory,
    worktree: directory,
    abort: new AbortController().signal,
    metadata: () => {},
    ask: async () => {},
    callID: `${sessionID}-call`,
  } as ToolContext;
}

async function runBash(
  hooks: PluginHooks,
  directory: string,
  sessionID: string,
  command: string,
): Promise<string> {
  const bash = hooks.tool.bash;
  if (!bash) throw new Error("real plugin factory did not register the bash tool");
  const result = await bash.execute(
    { command, wait: true, timeout: 45_000, compressed: false },
    toolContext(directory, sessionID),
  );
  return typeof result === "string"
    ? result
    : String((result as { output?: unknown }).output ?? "");
}

maybeDescribe("OpenCode plugin instance disposal", () => {
  test("disposing A interrupts only A while B's held call and pool stay live", async () => {
    const tempRoot = await mkdtemp(join(tmpdir(), "aft-opencode-instance-disposal-"));
    const directoryA = join(tempRoot, "project-a");
    const directoryB = join(tempRoot, "project-b");
    const xdgConfigHome = join(tempRoot, "config");
    const storageDir = join(tempRoot, "data", "cortexkit", "aft");
    await Promise.all([
      mkdir(directoryA, { recursive: true }),
      mkdir(directoryB, { recursive: true }),
      mkdir(join(xdgConfigHome, "cortexkit"), { recursive: true }),
    ]);
    await writeFile(
      join(xdgConfigHome, "cortexkit", "aft.jsonc"),
      JSON.stringify({
        auto_update: false,
        backup: { enabled: false },
        bash: { background: true, compress: false, host_fallback: false, rewrite: false },
        lsp: { auto_install: false },
        restrict_to_project_root: true,
        sandbox: { enabled: false },
        search_index: false,
        semantic_search: false,
      }),
      "utf8",
    );

    const releaseEnv = await acquireEnv({
      AFT_BINARY_PATH: AFT_BINARY,
      AFT_CACHE_DIR: join(tempRoot, "cache", "aft"),
      AFT_STORAGE_DIR: storageDir,
      HOME: join(tempRoot, "home"),
      OPENCODE_CONFIG_DIR: join(tempRoot, "opencode-config"),
      XDG_CACHE_HOME: join(tempRoot, "cache"),
      XDG_CONFIG_HOME: xdgConfigHome,
      XDG_DATA_HOME: join(tempRoot, "data"),
      XDG_STATE_HOME: join(tempRoot, "state"),
    });

    let hooksA: PluginHooks | undefined;
    let hooksB: PluginHooks | undefined;
    let shutdownHooks: ShutdownHooksModule | undefined;
    let restoreLogger: (() => void) | undefined;
    try {
      delete (globalThis as unknown as Record<string, unknown>).__aftShutdownHooks__;
      const nonce = `${Date.now()}-${Math.random()}`;
      const pluginModule = await import(`../../index.js?instance-disposal-${nonce}`);
      const loggerModule = await import("../../logger.js");
      shutdownHooks = await import("../../shutdown-hooks.js");
      const warnings: string[] = [];
      setActiveLogger({
        log: () => {},
        warn: (message) => warnings.push(message),
        error: () => {},
      });
      restoreLogger = () => setActiveLogger(loggerModule.bridgeLogger);

      const plugin = pluginModule.default;
      const beforeA = new Set(cleanupRegistry());
      hooksA = (await plugin({ directory: directoryA, client: {} } as Parameters<
        typeof plugin
      >[0])) as PluginHooks;
      const cleanupA = onlyNewCleanup(beforeA);

      const beforeB = new Set(cleanupRegistry());
      hooksB = (await plugin({ directory: directoryB, client: {} } as Parameters<
        typeof plugin
      >[0])) as PluginHooks;
      const cleanupB = onlyNewCleanup(beforeB);

      expect(await runBash(hooksA, directoryA, "session-a-warm", warmCommand("warm-a"))).toContain(
        "warm-a",
      );
      expect(await runBash(hooksB, directoryB, "session-b-warm", warmCommand("warm-b"))).toContain(
        "warm-b",
      );

      const markerA = join(directoryA, "held.started");
      const markerB = join(directoryB, "held.started");
      const heldA = runBash(
        hooksA,
        directoryA,
        "session-a-held",
        shellCommand(markerA, 30, "unexpected-a-completion"),
      ).then(
        (output) => ({ output }),
        (error: unknown) => ({ error }),
      );
      const heldB = runBash(
        hooksB,
        directoryB,
        "session-b-held",
        shellCommand(markerB, 5, "completed-b"),
      ).then(
        (output) => ({ output }),
        (error: unknown) => ({ error }),
      );
      await Promise.all([waitForFile(markerA), waitForFile(markerB)]);
      warnings.length = 0;

      await hooksA.dispose();

      const aOutcome = await heldA;
      expect("error" in aOutcome).toBe(true);
      expect(String("error" in aOutcome ? aOutcome.error : "")).toContain("Bridge shutting down");

      const bOutcome = await heldB;
      expect("error" in bOutcome ? String(bOutcome.error) : bOutcome.output).toContain(
        "completed-b",
      );
      expect("error" in bOutcome).toBe(false);

      expect(cleanupRegistry().has(cleanupA)).toBe(false);
      expect(cleanupRegistry().has(cleanupB)).toBe(true);
      expect(shutdownHooks.__shutdownCleanupInvocationCountForTests(cleanupA)).toBe(1);
      expect(shutdownHooks.__shutdownCleanupInvocationCountForTests(cleanupB)).toBe(0);
      expect(
        await runBash(hooksB, directoryB, "session-b-after-a", warmCommand("still-live-b")),
      ).toContain("still-live-b");
      expect(warnings.filter((message) => message.includes("transport was shut down"))).toEqual([]);

      await hooksA.dispose();
      expect(shutdownHooks.__shutdownCleanupInvocationCountForTests(cleanupA)).toBe(1);

      await hooksB.dispose();
      expect(cleanupRegistry().has(cleanupB)).toBe(false);
      expect(shutdownHooks.__shutdownCleanupInvocationCountForTests(cleanupA)).toBe(1);
      expect(shutdownHooks.__shutdownCleanupInvocationCountForTests(cleanupB)).toBe(1);

      await hooksB.dispose();
      await shutdownHooks.runCleanups("beforeExit");
      expect(shutdownHooks.__shutdownCleanupInvocationCountForTests(cleanupA)).toBe(1);
      expect(shutdownHooks.__shutdownCleanupInvocationCountForTests(cleanupB)).toBe(1);
    } finally {
      await Promise.allSettled([
        hooksA?.dispose(),
        hooksB?.dispose(),
        shutdownHooks?.runCleanups("test teardown"),
      ]);
      restoreLogger?.();
      delete (globalThis as unknown as Record<string, unknown>).__aftShutdownHooks__;
      releaseEnv();
      await rm(tempRoot, { recursive: true, force: true });
    }
  }, 60_000);
});
