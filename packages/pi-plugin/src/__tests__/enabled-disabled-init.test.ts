/// <reference path="../bun-test.d.ts" />

/**
 * An unusable configuration must not leave Pi without AFT silently. The
 * extension loads in the config error state instead: AFT's tools register,
 * every tool call fails with the error and its fix, the status bar shows the
 * error on one line, and no binary or bridge is started. (A rejected
 * configuration used to register nothing, and a missing subc connection file
 * made the extension factory throw.)
 */

import { afterEach, beforeAll, describe, expect, mock, spyOn, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import * as bridge from "@cortexkit/aft-bridge";
import { acquireEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import * as logger from "../logger.js";

type PiPlugin = typeof import("../index.js").default;
type RegisteredTool = {
  name: string;
  execute(...args: unknown[]): Promise<unknown>;
};

let importCounter = 0;
let releaseEnv: (() => void) | undefined;
let tempDir: string | undefined;
let previousCwd: string | undefined;

async function loadPlugin(): Promise<PiPlugin> {
  const mod = await import(`../index.js?config-error=${importCounter++}`);
  return mod.default;
}

// The extension module graph is large; transpile it once up front so the
// first test does not spend its whole timeout on a cold import.
beforeAll(async () => {
  await import("../index.js");
}, 120_000);

afterEach(() => {
  if (previousCwd) process.chdir(previousCwd);
  previousCwd = undefined;
  releaseEnv?.();
  releaseEnv = undefined;
  if (tempDir) rmSync(tempDir, { recursive: true, force: true });
  tempDir = undefined;
  mock.restore();
});

type Layout = { projectDir: string; userConfig: string };

async function sandbox(): Promise<Layout> {
  tempDir = mkdtempSync(join(tmpdir(), "aft-pi-config-error-"));
  const projectDir = join(tempDir, "project");
  mkdirSync(join(projectDir, ".cortexkit"), { recursive: true });
  mkdirSync(join(tempDir, "config", "cortexkit"), { recursive: true });
  previousCwd = process.cwd();
  process.chdir(projectDir);
  releaseEnv = await acquireEnv({
    // CI exports an ambient AFT_CACHE_DIR; it outranks XDG_CACHE_HOME in the
    // shared cache resolver, so clear it for the sandbox to apply.
    AFT_CACHE_DIR: undefined,
    AFT_STORAGE_DIR: undefined,
    HOME: join(tempDir, "home"),
    XDG_CONFIG_HOME: join(tempDir, "config"),
    XDG_CACHE_HOME: join(tempDir, "cache"),
    XDG_DATA_HOME: join(tempDir, "data"),
  });
  return { projectDir, userConfig: join(tempDir, "config", "cortexkit", "aft.jsonc") };
}

/** Boot the extension with spies proving nothing is resolved or spawned. */
async function bootInErrorState() {
  const errorSpy = spyOn(logger, "error");
  const findBinarySpy = spyOn(bridge, "findBinary").mockImplementation(async () => {
    throw new Error("findBinary must not run in the config error state");
  });
  const createPoolSpy = spyOn(bridge, "createAftTransportPool").mockImplementation(async () => {
    throw new Error("createAftTransportPool must not run in the config error state");
  });
  const tools = new Map<string, RegisteredTool>();
  const handlers = new Map<string, (event: unknown, ctx: unknown) => unknown>();
  const pi = {
    registerTool: (tool: RegisteredTool) => tools.set(tool.name, tool),
    registerCommand: mock(() => undefined),
    registerMessageRenderer: mock(() => undefined),
    on: (event: string, handler: (event: unknown, ctx: unknown) => unknown) =>
      handlers.set(event, handler),
    ui: { notify: mock(() => undefined) },
  };

  const plugin = await loadPlugin();
  await plugin(pi as unknown as Parameters<PiPlugin>[0]);

  expect(findBinarySpy).not.toHaveBeenCalled();
  expect(createPoolSpy).not.toHaveBeenCalled();

  const statuses: Array<[string, string | undefined]> = [];
  handlers.get("session_start")?.(
    {},
    { ui: { setStatus: (key: string, text: string | undefined) => statuses.push([key, text]) } },
  );
  return { tools, statuses, errorSpy };
}

async function expectFailingCall(tools: Map<string, RegisteredTool>, fix: string) {
  const read = tools.get("read");
  expect(read).toBeDefined();
  const call = () => read?.execute("call-1", { path: "a.ts" }, undefined, undefined, {});
  await expect(call()).rejects.toThrow(fix);
  await expect(call()).rejects.toThrow("restart");
}

describe.serial("Pi config error state", () => {
  test("a missing subc connection file registers the tools and fails every call", async () => {
    const layout = await sandbox();
    const missing = join(tempDir as string, "no-such-connection.json");
    writeFileSync(layout.userConfig, JSON.stringify({ subc: { connection_file: missing } }));

    const { tools, statuses, errorSpy } = await bootInErrorState();

    expect([...tools.keys()]).toContain("aft_outline");
    await expectFailingCall(
      tools,
      "Start the Subconscious daemon, correct the path, or remove subc.connection_file",
    );
    expect(statuses).toHaveLength(1);
    expect(statuses[0]?.[1]).toStartWith("AFT config error: subc.connection_file");
    expect(statuses[0]?.[1]).not.toContain("\n");
    const errors = errorSpy.mock.calls.map((call) => String(call[0]));
    // Logged once at startup, not once per call.
    expect(errors.filter((line) => line.includes("no subc connection file"))).toHaveLength(1);
  });

  test("a rejected legacy key registers the default surface and fails every call", async () => {
    const layout = await sandbox();
    // An already retired alias rejects the whole load.
    writeFileSync(
      join(layout.projectDir, ".cortexkit", "aft.jsonc"),
      '{ "gh_read": { "enabled": true } }\n',
    );

    const { tools, statuses } = await bootInErrorState();

    expect([...tools.keys()]).not.toContain("aft_move");
    await expectFailingCall(tools, "npx @cortexkit/aft doctor --fix");
    await expectFailingCall(tools, "removed_config_key:gh_read:use:github.read");
    expect(statuses[0]?.[1]).toContain("removed_config_key:gh_read");
  });

  test("a config file that does not parse registers the default surface and fails every call", async () => {
    const layout = await sandbox();
    writeFileSync(join(layout.projectDir, ".cortexkit", "aft.jsonc"), '{ "edit_mode": "hashline"\n');

    const { tools } = await bootInErrorState();

    await expectFailingCall(tools, "failed to parse");
    await expectFailingCall(tools, "Fix the JSONC syntax in that file");
  });
});
