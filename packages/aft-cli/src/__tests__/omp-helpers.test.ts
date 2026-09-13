/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { chmodSync, mkdirSync, mkdtempSync, rmSync, unlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { acquireEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import {
  detectOmpBinary,
  getOmpCommandInvocation,
  getOmpFallbackCandidates,
  OMP_HOST_PACKAGE,
} from "../lib/omp-helpers.js";

let root: string;
let releaseEnv: (() => void) | undefined;

function executable(path: string): string {
  mkdirSync(join(path, ".."), { recursive: true });
  writeFileSync(path, "#!/bin/sh\nexit 0\n");
  chmodSync(path, 0o755);
  return path;
}

function binaryName(name: string): string {
  return process.platform === "win32" ? `${name}.exe` : name;
}

beforeEach(async () => {
  root = mkdtempSync(join(tmpdir(), "aft-cli-omp-discovery-"));
  releaseEnv = await acquireEnv({
    HOME: join(root, "home"),
    USERPROFILE: join(root, "home"),
    XDG_CONFIG_HOME: undefined,
    XDG_DATA_HOME: undefined,
    PI_CONFIG_DIR: undefined,
    PI_CODING_AGENT_DIR: undefined,
    PI_PACKAGE_DIR: undefined,
    PI_PROFILE: undefined,
    PI_CONFIG_FILES: undefined,
    OMP_PROFILE: undefined,
    APPDATA: undefined,
    PATH: join(root, "empty-bin"),
  });
});

afterEach(() => {
  releaseEnv?.();
  releaseEnv = undefined;
  rmSync(root, { recursive: true, force: true });
});

describe("OMP binary discovery", () => {
  test("uses PATH before a Bun-runnable package CLI and home fallbacks", () => {
    const binDir = join(root, "bin");
    const pathOmp = executable(join(binDir, binaryName("omp")));
    executable(join(binDir, binaryName("bun")));
    process.env.PATH = binDir;

    const packageDir = join(root, "package");
    mkdirSync(join(packageDir, "dist"), { recursive: true });
    writeFileSync(join(packageDir, "package.json"), JSON.stringify({ name: OMP_HOST_PACKAGE }));
    const packageCli = join(packageDir, "dist", "cli.js");
    writeFileSync(packageCli, "");
    process.env.PI_PACKAGE_DIR = packageDir;

    const homeOmp = executable(
      process.platform === "win32"
        ? join(process.env.HOME!, ".bun", "bin", "omp.exe")
        : join(process.env.HOME!, ".bun", "bin", "omp"),
    );

    expect(detectOmpBinary()).toEqual({ path: pathOmp, source: "path" });
    unlinkSync(pathOmp);
    expect(detectOmpBinary()).toEqual({ path: packageCli, source: "package" });
    unlinkSync(packageCli);
    expect(detectOmpBinary()).toEqual({ path: homeOmp, source: "home" });
  });

  test("ignores PI_PACKAGE_DIR when Bun is unavailable", () => {
    const packageDir = join(root, "package");
    mkdirSync(join(packageDir, "dist"), { recursive: true });
    writeFileSync(join(packageDir, "package.json"), JSON.stringify({ name: OMP_HOST_PACKAGE }));
    writeFileSync(join(packageDir, "dist", "cli.js"), "");
    process.env.PI_PACKAGE_DIR = packageDir;

    expect(detectOmpBinary()).toBeNull();
  });

  test("routes a package CLI through Bun", () => {
    const binDir = join(root, "bin");
    const bun = executable(join(binDir, binaryName("bun")));
    process.env.PATH = binDir;
    const cli = join(root, "package", "dist", "cli.js");

    expect(getOmpCommandInvocation(cli, ["--version"])).toEqual({
      command: bun,
      args: [cli, "--version"],
    });
  });

  test("lists Windows npm and Bun home candidates in host order", () => {
    const home = "C:\\Users\\fox";
    const appData = "C:\\Users\\fox\\AppData\\Roaming";

    expect(getOmpFallbackCandidates("win32", home, appData)).toEqual([
      join(appData, "npm", "omp.cmd"),
      join(appData, "npm", "omp.exe"),
      join(home, ".bun", "bin", "omp.exe"),
      join(home, ".bun", "bin", "omp.cmd"),
    ]);
  });
});
