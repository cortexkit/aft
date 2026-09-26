/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { type SpawnSyncReturns, spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { linkCachedExecutable } from "../../../aft-bridge/src/__tests__/test-utils/cached-executable.js";
import { getAftBinaryName } from "../lib/paths.js";

/** Exit code the fixture binary returns, so no other `aft` can fake a pass. */
const FIXTURE_EXIT_CODE = 7;

const CLI_ENTRY = fileURLToPath(new URL("../index.ts", import.meta.url));

/**
 * Run the CLI in a child process whose environment contains only a temporary
 * cache, an empty PATH and a temporary HOME.
 *
 * A child process is what makes the fixture honest: `os.homedir()` is read
 * from the environment the process started with, so the `~/.cargo/bin` lookup
 * can only be pointed at an empty directory before launch. Running in-process
 * would leave the developer's real cargo install in play.
 */
function runCli(args: string[], env: Record<string, string>): SpawnSyncReturns<string> {
  return spawnSync(process.execPath, [CLI_ENTRY, ...args], {
    encoding: "utf-8",
    env,
  });
}

interface CacheOnlyInstall {
  env: Record<string, string>;
  cacheBinDir: string;
  cargoBinary: string;
}

/**
 * Reproduce the reported container: a binary only under
 * `<cache>/v<semver>/<name>`, with no `aft` on PATH, no npm platform package
 * and no cargo install.
 */
function cacheOnlyInstall(prefix: string, versionDir: string | null): CacheOnlyInstall {
  const root = mkdtempSync(join(tmpdir(), prefix));
  const cacheRoot = join(root, "cache");
  const cacheBinDir = join(cacheRoot, "bin");
  const home = join(root, "home");
  const emptyPathDir = join(root, "empty-path");
  mkdirSync(cacheBinDir, { recursive: true });
  mkdirSync(home, { recursive: true });
  mkdirSync(emptyPathDir, { recursive: true });

  if (versionDir) {
    const versionedDir = join(cacheBinDir, versionDir);
    mkdirSync(versionedDir, { recursive: true });
    const binary = join(versionedDir, getAftBinaryName());
    linkCachedExecutable(binary, `#!/bin/sh\nexit ${FIXTURE_EXIT_CODE}\n`);
  }

  return {
    env: { AFT_CACHE_DIR: cacheRoot, HOME: home, PATH: emptyPathDir },
    cacheBinDir,
    cargoBinary: join(home, ".cargo", "bin", getAftBinaryName()),
  };
}

describe("commands that need the native binary", () => {
  test("aft index runs the binary that doctor --fix left in the cache", async () => {
    const install = cacheOnlyInstall("aft-cli-index-cache-only-", "v0.56.2");

    const result = runCli(["index"], install.env);

    expect(result.stderr).not.toContain("requires a native AFT binary");
    expect(result.status).toBe(FIXTURE_EXIT_CODE);
  });

  test("aft doctor --profile runs the binary that doctor --fix left in the cache", async () => {
    const install = cacheOnlyInstall("aft-cli-profile-cache-only-", "v0.56.2");

    const result = runCli(["doctor", "--profile", "4"], install.env);

    expect(result.stderr).not.toContain("requires a native AFT binary");
    expect(result.status).toBe(FIXTURE_EXIT_CODE);
  });

  test("aft index reports every location it searched when no binary exists", async () => {
    const install = cacheOnlyInstall("aft-cli-index-missing-", null);

    const result = runCli(["index"], install.env);

    expect(result.status).toBe(1);
    expect(result.stderr).toContain(install.cacheBinDir);
    expect(result.stderr).toContain("npm platform package");
    expect(result.stderr).toContain("PATH");
    expect(result.stderr).toContain(install.cargoBinary);
  });
});
