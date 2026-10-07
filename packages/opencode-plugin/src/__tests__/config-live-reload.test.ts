/// <reference path="../bun-test.d.ts" />

/**
 * Live config reload in the OpenCode plugins: an edit to either config file
 * reaches the plugin's own `ctx.config` for the keys it reads per call, and
 * leaves everything that shapes the registered tools as loaded.
 */

import { afterEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, unlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { acquireEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import {
  getConfigLoadSources,
  getConfigLoadTexts,
  loadAftConfig,
  resolveBashConfig,
} from "../config.js";
import { startOpenCodeLiveConfigReload } from "../config-live-reload.js";
import { createBashWatchTool } from "../tools/bash_watch.js";
import type { PluginContext } from "../types.js";
import { noopAsk } from "./test-helpers";

const PROJECT_CWD = resolve(import.meta.dir, "../../../..");
const cleanup: (() => void)[] = [];

afterEach(() => {
  for (const fn of cleanup.splice(0).reverse()) fn();
});

async function fixture(user: string, project?: string) {
  const root = mkdtempSync(join(tmpdir(), "aft-oc-live-config-"));
  const xdg = join(root, "xdg");
  const projectDir = join(root, "project");
  mkdirSync(join(xdg, "cortexkit"), { recursive: true });
  mkdirSync(join(projectDir, ".cortexkit"), { recursive: true });
  const userPath = join(xdg, "cortexkit", "aft.jsonc");
  const projectPath = join(projectDir, ".cortexkit", "aft.jsonc");
  writeFileSync(userPath, user);
  if (project !== undefined) writeFileSync(projectPath, project);
  const releaseEnv = await acquireEnv({ HOME: join(root, "home"), XDG_CONFIG_HOME: xdg });
  cleanup.push(() => rmSync(root, { recursive: true, force: true }));
  cleanup.push(releaseEnv);

  const bridge = {
    send: async () => ({ success: true, status: "completed", exit_code: 0 }),
  };
  const ctx: PluginContext = {
    pool: { getBridge: () => bridge } as unknown as BridgePool,
    client: {} as PluginContext["client"],
    config: loadAftConfig(projectDir),
    storageDir: join(root, "storage"),
  };
  const notices: string[] = [];
  const reload = startOpenCodeLiveConfigReload({
    directory: projectDir,
    initialSources: [...getConfigLoadSources()],
    initialSourceTexts: Object.fromEntries(getConfigLoadTexts()),
    getConfig: () => ctx.config,
    setConfig: (next) => {
      ctx.config = next;
    },
    notify: (message) => notices.push(message),
    watch: false,
  });
  cleanup.push(() => reload.stop());
  return { ctx, reload, notices, userPath, projectPath };
}

function sdkContext(): ToolContext {
  return {
    sessionID: "live-config-session",
    messageID: "m",
    agent: "a",
    directory: PROJECT_CWD,
    worktree: PROJECT_CWD,
    abort: new AbortController().signal,
    metadata: () => {},
    ask: noopAsk,
    callID: "c",
  } as ToolContext;
}

describe.serial("OpenCode live config reload", () => {
  test("inspect categories and LSP idle minutes follow trusted user edits live", async () => {
    const f = await fixture(
      '{"lsp":{"idle_minutes":60},"inspect":{"categories":{"dead_code":true}}}',
    );
    writeFileSync(
      f.userPath,
      '{"lsp":{"idle_minutes":"never"},"inspect":{"categories":{"dead_code":false}}}',
    );
    const result = f.reload.reload();
    expect(result?.applied).toContain("lsp.idle_minutes");
    expect(result?.applied).toContain("inspect.categories.dead_code");
    expect(result?.deferred).toEqual([]);
    expect(f.ctx.config.lsp?.idle_minutes).toBe("never");
    expect(f.ctx.config.inspect?.categories?.dead_code).toBe(false);
  });
  test("a bash_watch cap edit reaches the next call; a restart-only key does not", async () => {
    const f = await fixture('{ "bash": { "watch_sync_max_ms": 120000, "background": true } }');
    const watchTool = createBashWatchTool(f.ctx);
    await expect(
      watchTool.execute({ taskId: "t", timeoutMs: 5_000 }, sdkContext()),
    ).resolves.toContain("task exited");

    writeFileSync(f.userPath, '{ "bash": { "watch_sync_max_ms": 1000, "background": false } }');
    const result = f.reload.reload();

    expect(result?.applied).toEqual(["bash.watch_sync_max_ms"]);
    expect(result?.deferred).toContain("bash.background");
    await expect(
      watchTool.execute({ taskId: "t", timeoutMs: 5_000 }, sdkContext()),
    ).rejects.toThrow("timeoutMs must be between 1 and 1000 (bash.watch_sync_max_ms)");
    // A `background` change is deferred until restart because it changes the
    // bash schema the model sees.
    expect(resolveBashConfig(f.ctx.config).background).toBe(true);
  });

  test("restrict_to_project_root follows a user file edit", async () => {
    const f = await fixture('{ "restrict_to_project_root": false }');
    writeFileSync(f.userPath, '{ "restrict_to_project_root": true }');
    f.reload.reload();
    expect(f.ctx.config.restrict_to_project_root).toBe(true);
  });

  test("an invalid edit keeps the last valid config and tells the user once", async () => {
    const f = await fixture("{}", '{ "configure_warnings_delivery": "chat" }');
    expect(f.ctx.config.configure_warnings_delivery).toBe("chat");

    writeFileSync(f.projectPath, '{ "configure_warnings_delivery": "toast" ');
    f.reload.reload();
    f.reload.reload();

    expect(f.ctx.config.configure_warnings_delivery).toBe("chat");
    expect(f.notices).toHaveLength(1);
    expect(f.notices[0]).toContain("failed to parse");
    expect(f.notices[0]).toContain("keeps using the last valid configuration");
  });

  test("one invalid value keeps the whole last valid config", async () => {
    const f = await fixture('{ "restrict_to_project_root": true }');
    writeFileSync(
      f.userPath,
      '{ "restrict_to_project_root": "yes", "bash": { "watch_sync_max_ms": 1000 } }',
    );
    f.reload.reload();
    expect(f.ctx.config.restrict_to_project_root).toBe(true);
    expect(resolveBashConfig(f.ctx.config).watch_sync_max_ms).toBe(120_000);
    expect(f.notices[0]).toContain("invalid setting");
  });

  test("a deleted user file keeps a security setting", async () => {
    const f = await fixture('{ "restrict_to_project_root": true }');
    unlinkSync(f.userPath);
    f.reload.reload();
    expect(f.ctx.config.restrict_to_project_root).toBe(true);
    expect(f.notices[0]).toContain("was deleted");
  });

  test("a project edit cannot turn host fallback on until the next restart", async () => {
    const f = await fixture("{}", '{ "bash": { "host_fallback": false } }');
    expect(resolveBashConfig(f.ctx.config).host_fallback).toBe(false);
    writeFileSync(f.projectPath, '{ "bash": { "host_fallback": true } }');
    const result = f.reload.reload();
    expect(result?.held).toEqual(["bash.host_fallback"]);
    expect(resolveBashConfig(f.ctx.config).host_fallback).toBe(false);

    // A later user-file edit keeps holding it.
    writeFileSync(f.userPath, '{ "bash": { "watch_sync_max_ms": 5000 } }');
    f.reload.reload();
    expect(resolveBashConfig(f.ctx.config).host_fallback).toBe(false);
  });

  test("a project edit can tighten host fallback", async () => {
    const f = await fixture("{}", '{ "bash": { "host_fallback": true } }');
    expect(resolveBashConfig(f.ctx.config).host_fallback).toBe(true);
    writeFileSync(f.projectPath, '{ "bash": { "host_fallback": false } }');
    expect(f.reload.reload()?.applied).toEqual(["bash.host_fallback"]);
    expect(resolveBashConfig(f.ctx.config).host_fallback).toBe(false);
  });

  test("the project tier still cannot loosen a user-only key", async () => {
    const f = await fixture('{ "restrict_to_project_root": true }', "{}");
    writeFileSync(f.projectPath, '{ "restrict_to_project_root": false }');
    f.reload.reload();
    expect(f.ctx.config.restrict_to_project_root).toBe(true);
  });
});
