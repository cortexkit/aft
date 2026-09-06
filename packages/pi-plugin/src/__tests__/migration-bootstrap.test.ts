/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { acquireEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";

type PiPlugin = typeof import("../index.js").default;

// Match PLUGIN_VERSION from package.json so the fake binary the resolver finds
// satisfies the version-match expectation in findBinary. Without this, every
// release that bumps the plugin version breaks the test because the resolver
// rejects the cached fake at the old version path.
const PLUGIN_VERSION: string = (() => {
  try {
    // eslint-disable-next-line @typescript-eslint/no-require-imports
    return (require("../../package.json") as { version: string }).version;
  } catch {
    return "0.0.0";
  }
})();

describe.serial("Pi migration bootstrap", () => {
  let tempDir: string;
  let projectDir: string;
  let prevCwd: string;
  let releaseEnv: (() => void) | undefined;
  let argsLog: string;
  let aftPath: string;
  let cachedAft: string;

  function writeFakeAft(exitCode: number): void {
    const contents = `#!/bin/sh\nif [ "$1" = "--version" ]; then echo "aft ${PLUGIN_VERSION}"; exit 0; fi\nprintf "%s\\n" "$@" >> ${JSON.stringify(argsLog)}\nexit ${exitCode}\n`;
    writeFileSync(aftPath, contents, "utf8");
    chmodSync(aftPath, 0o755);
    writeFileSync(cachedAft, contents, "utf8");
    chmodSync(cachedAft, 0o755);
  }

  beforeEach(async () => {
    tempDir = mkdtempSync(join(tmpdir(), "aft-pi-migration-bootstrap-"));
    projectDir = join(tempDir, "project");
    mkdirSync(projectDir, { recursive: true });
    prevCwd = process.cwd();

    const binDir = join(tempDir, "bin");
    mkdirSync(binDir, { recursive: true });
    argsLog = join(tempDir, "args.log");
    aftPath = join(binDir, "aft");

    const home = join(tempDir, "home");
    const xdgCacheHome = join(tempDir, "cache");
    releaseEnv = await acquireEnv({
      // CI exports an ambient AFT_CACHE_DIR; it outranks XDG_CACHE_HOME in the
      // shared cache resolver, so clear it for the sandbox to apply.
      AFT_CACHE_DIR: undefined,
      AFT_STORAGE_DIR: undefined,
      // CI also exports the built binary here, and since it became resolution
      // step 0 it would win over the fake `aft` this test plants on PATH.
      AFT_BINARY_PATH: undefined,
      PATH: `${binDir}:${process.env.PATH ?? ""}`,
      HOME: home,
      XDG_DATA_HOME: join(tempDir, "data"),
      XDG_CACHE_HOME: xdgCacheHome,
      AFT_MIGRATION_ARGS_LOG: argsLog,
    });

    cachedAft = join(xdgCacheHome, "aft", "bin", `v${PLUGIN_VERSION}`, "aft");
    mkdirSync(join(xdgCacheHome, "aft", "bin", `v${PLUGIN_VERSION}`), { recursive: true });
    writeFakeAft(0);

    mkdirSync(join(home, ".pi", "agent"), { recursive: true });
    writeFileSync(
      join(home, ".pi", "agent", "aft.json"),
      JSON.stringify({ lsp: { auto_install: false }, semantic_search: false }),
      "utf8",
    );
    process.chdir(projectDir);
  });

  afterEach(() => {
    process.chdir(prevCwd);
    releaseEnv?.();
    releaseEnv = undefined;
    rmSync(tempDir, { recursive: true, force: true });
  });

  function createLegacyRoot(): string {
    const legacyRoot = join(process.env.HOME as string, ".pi", "agent", "aft");
    mkdirSync(legacyRoot, { recursive: true });
    writeFileSync(join(legacyRoot, "warned_tools.json"), "{}", "utf8");
    return legacyRoot;
  }

  async function loadPlugin(): Promise<PiPlugin> {
    const mod = await import(`../index.js?migration-bootstrap-${Date.now()}-${Math.random()}`);
    return mod.default;
  }

  function makePi(): Parameters<PiPlugin>[0] {
    return {
      registerTool: () => {},
      registerCommand: () => {},
      on: () => {},
    } as Parameters<PiPlugin>[0];
  }

  test("pi_plugin_calls_ensureStorageMigrated_with_pi_harness", async () => {
    const legacyRoot = createLegacyRoot();
    const plugin = await loadPlugin();

    await plugin(makePi());

    const argv = readFileSync(argsLog, "utf8").trim().split("\n");
    expect(argv).toContain("migrate-storage");
    expect(argv).toContain("--harness");
    expect(argv).toContain("pi");
    expect(argv).toContain("--from");
    expect(argv).toContain(legacyRoot);
    expect(argv).toContain("--to");
    expect(argv).toContain(join(process.env.XDG_DATA_HOME as string, "cortexkit", "aft"));
  });

  test("pi_plugin_aborts_on_migration_error", async () => {
    createLegacyRoot();
    writeFakeAft(5);
    const plugin = await loadPlugin();

    await expect(plugin(makePi())).rejects.toThrow(/AFT storage migration failed.*exit 5/);
  });
});
