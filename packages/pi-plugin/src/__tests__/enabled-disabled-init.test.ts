/// <reference path="../bun-test.d.ts" />

import { afterEach, describe, expect, mock, spyOn, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import * as bridge from "@cortexkit/aft-bridge";
import { acquireEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import { getLogFilePath } from "../logger.js";

type PiPlugin = typeof import("../index.js").default;

let importCounter = 0;
let releaseEnv: (() => void) | undefined;
let tempDir: string | undefined;
let previousCwd: string | undefined;

async function loadPlugin(): Promise<PiPlugin> {
  const mod = await import(`../index.js?enabled-disabled=${importCounter++}`);
  return mod.default;
}

afterEach(() => {
  if (previousCwd) process.chdir(previousCwd);
  previousCwd = undefined;
  releaseEnv?.();
  releaseEnv = undefined;
  if (tempDir) rmSync(tempDir, { recursive: true, force: true });
  tempDir = undefined;
  mock.restore();
});

describe.serial("Pi rejected config", () => {
  test("rejected config registers nothing without resolving a binary or creating a bridge pool", async () => {
    tempDir = mkdtempSync(join(tmpdir(), "aft-pi-disabled-"));
    const projectDir = join(tempDir, "project");
    mkdirSync(join(projectDir, ".cortexkit"), { recursive: true });
    // The already-retired alias rejects the whole candidate load; there is no
    // longer a config switch that disables AFT wholesale.
    writeFileSync(
      join(projectDir, ".cortexkit", "aft.jsonc"),
      '{ "gh_read": { "enabled": true } }\n',
    );
    previousCwd = process.cwd();
    process.chdir(projectDir);
    const loggedProjectDir = process.cwd();
    releaseEnv = await acquireEnv({
      // CI exports an ambient AFT_CACHE_DIR; it outranks XDG_CACHE_HOME in the
      // shared cache resolver, so clear it for the sandbox to apply.
      AFT_CACHE_DIR: undefined,
      HOME: join(tempDir, "home"),
      XDG_CONFIG_HOME: join(tempDir, "config"),
      XDG_CACHE_HOME: join(tempDir, "cache"),
      XDG_DATA_HOME: join(tempDir, "data"),
    });
    const logFile = getLogFilePath();
    rmSync(logFile, { force: true });
    const findBinarySpy = spyOn(bridge, "findBinary").mockImplementation(async () => {
      throw new Error("findBinary should not run when AFT is disabled");
    });
    const createPoolSpy = spyOn(bridge, "createAftTransportPool").mockImplementation(async () => {
      throw new Error("createAftTransportPool should not run when AFT is disabled");
    });
    const registerTool = mock(() => undefined);
    const registerCommand = mock(() => undefined);
    const on = mock(() => undefined);

    const plugin = await loadPlugin();
    await plugin({ registerTool, registerCommand, on } as unknown as Parameters<PiPlugin>[0]);

    expect(registerTool).not.toHaveBeenCalled();
    expect(registerCommand).not.toHaveBeenCalled();
    expect(on).not.toHaveBeenCalled();
    expect(findBinarySpy).not.toHaveBeenCalled();
    expect(createPoolSpy).not.toHaveBeenCalled();
    await new Promise((resolve) => setTimeout(resolve, 600));
    const logText = existsSync(logFile) ? readFileSync(logFile, "utf8") : "";
    expect(logText).toContain("removed_config_key:gh_read:use:github.read");
    expect(loggedProjectDir).toBeTruthy();
  });
});
