/// <reference path="../bun-test.d.ts" />

/**
 * An unusable configuration must not stop OpenCode 1 from loading the plugin.
 * The plugin loads in the config error state instead: AFT's tool surface
 * registers, every tool call fails with the error and its fix, and no bridge
 * or binary is started. (A rejected configuration used to publish no tools at
 * all, and a missing subc connection file made init throw, which hosts
 * record only in their own log file.)
 */

import { afterEach, describe, expect, mock, spyOn, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import * as bridge from "@cortexkit/aft-bridge";
import { acquireEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import * as logger from "../logger.js";

type OpenCodePlugin = typeof import("../index.js").default;
type Hooks = {
  tool?: Record<string, { execute(args: unknown, ctx: unknown): Promise<unknown> }>;
  dispose?: () => Promise<void>;
};

let importCounter = 0;
let releaseEnv: (() => void) | undefined;
let tempDir: string | undefined;
let hooks: Hooks | undefined;

async function loadPlugin(): Promise<OpenCodePlugin> {
  const mod = await import(`../index.js?config-error=${importCounter++}`);
  return mod.default;
}

afterEach(async () => {
  await hooks?.dispose?.();
  hooks = undefined;
  releaseEnv?.();
  releaseEnv = undefined;
  if (tempDir) rmSync(tempDir, { recursive: true, force: true });
  tempDir = undefined;
  mock.restore();
});

type Layout = { projectDir: string; sessionDirectory: string; userConfig: string };

async function sandbox(): Promise<Layout> {
  tempDir = mkdtempSync(join(tmpdir(), "aft-opencode-config-error-"));
  const projectDir = join(tempDir, "project");
  const sessionDirectory = join(projectDir, "src", "nested");
  mkdirSync(sessionDirectory, { recursive: true });
  mkdirSync(join(projectDir, ".cortexkit"), { recursive: true });
  mkdirSync(join(tempDir, "config", "cortexkit"), { recursive: true });
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
  return {
    projectDir,
    sessionDirectory,
    userConfig: join(tempDir, "config", "cortexkit", "aft.jsonc"),
  };
}

/** Boot the plugin with spies proving nothing is resolved or spawned. */
async function bootInErrorState(layout: Layout) {
  // Assert the log CALL, not the log file: the logger buffers behind a
  // 500ms flush timer onto a file shared by every test in the process.
  const errorSpy = spyOn(logger, "error");
  const findBinarySpy = spyOn(bridge, "findBinary").mockImplementation(async () => {
    throw new Error("findBinary must not run in the config error state");
  });
  const createPoolSpy = spyOn(bridge, "createAftTransportPool").mockImplementation(async () => {
    throw new Error("createAftTransportPool must not run in the config error state");
  });

  const plugin = await loadPlugin();
  hooks = (await plugin({
    directory: layout.sessionDirectory,
    worktree: layout.projectDir,
    client: {},
  } as Parameters<OpenCodePlugin>[0])) as Hooks;

  expect(findBinarySpy).not.toHaveBeenCalled();
  expect(createPoolSpy).not.toHaveBeenCalled();
  return { tools: hooks.tool ?? {}, errorSpy };
}

async function expectFailingCall(tools: NonNullable<Hooks["tool"]>, fix: string): Promise<void> {
  await expect(
    tools.read.execute({ filePath: "a.ts" }, { sessionID: "ses_1", directory: "/x" }),
  ).rejects.toThrow(fix);
  await expect(
    tools.read.execute({ filePath: "a.ts" }, { sessionID: "ses_1", directory: "/x" }),
  ).rejects.toThrow("restart");
}

describe.serial("OpenCode config error state", () => {
  test("a missing subc connection file registers the tools and fails every call", async () => {
    const layout = await sandbox();
    const missing = join(tempDir as string, "no-such-connection.json");
    writeFileSync(layout.userConfig, JSON.stringify({ subc: { connection_file: missing } }));

    const { tools, errorSpy } = await bootInErrorState(layout);

    expect(Object.keys(tools)).toContain("read");
    expect(Object.keys(tools)).toContain("aft_outline");
    await expectFailingCall(
      tools,
      "Start the Subconscious daemon, correct the path, or remove subc.connection_file",
    );
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

    const { tools, errorSpy } = await bootInErrorState(layout);

    expect(Object.keys(tools)).toContain("read");
    expect(Object.keys(tools)).not.toContain("aft_move");
    await expectFailingCall(tools, "npx @cortexkit/aft doctor --fix");
    await expect(tools.read.execute({ filePath: "a.ts" }, {})).rejects.toThrow(
      "removed_config_key:gh_read:use:github.read",
    );
    const errors = errorSpy.mock.calls.map((call) => String(call[0]));
    expect(errors.filter((line) => line.includes("removed_config_key:gh_read"))).toHaveLength(1);
  });

  test("a config file that does not parse registers the default surface and fails every call", async () => {
    const layout = await sandbox();
    writeFileSync(
      join(layout.projectDir, ".cortexkit", "aft.jsonc"),
      '{ "edit_mode": "hashline"\n',
    );

    const { tools } = await bootInErrorState(layout);

    expect(Object.keys(tools)).toContain("read");
    await expectFailingCall(tools, "failed to parse");
    await expect(tools.read.execute({ filePath: "a.ts" }, {})).rejects.toThrow(
      "Fix the JSONC syntax in that file",
    );
  });
});
