/// <reference path="../bun-test.d.ts" />

/**
 * Pi plugin init must never execute the `aft` binary on the host thread.
 *
 * Pi runs extensions on one JavaScript thread. Asking a cached binary for
 * `--version` blocked it inside `posix_spawn` until the child was loaded,
 * which froze the host for up to a minute when the 85 MB binary had gone
 * cold. These tests boot the real extension against a cached stub `aft` that
 * records every time it is executed, and require that it was never run.
 *
 * Pi's eager bridge warmup (which spawns the bridge process itself, on
 * purpose) is skipped by starting from the home directory, so a recorded exec
 * can only come from binary resolution.
 */
import { afterEach, beforeEach, describe, expect, mock, spyOn, test } from "bun:test";
import {
  chmodSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import * as bridge from "@cortexkit/aft-bridge";
import { acquireEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import { writeBinaryIdentitySidecar } from "../../../aft-bridge/src/binary-identity.js";

type PiPlugin = typeof import("../index.js").default;

const PLUGIN_VERSION: string = (() => {
  try {
    return (require("../../package.json") as { version: string }).version;
  } catch {
    return "0.0.0";
  }
})();

describe.serial.skipIf(process.platform === "win32")(
  "Pi plugin init never executes the aft binary",
  () => {
    let tempDir: string;
    let home: string;
    let prevCwd: string;
    let execLog: string;
    let cachedAft: string;
    let releaseEnv: (() => void) | undefined;

    beforeEach(async () => {
      tempDir = mkdtempSync(join(tmpdir(), "aft-pi-no-exec-"));
      home = join(tempDir, "home");
      execLog = join(tempDir, "exec.log");
      const cacheHome = join(tempDir, "cache");
      releaseEnv = await acquireEnv({
        AFT_CACHE_DIR: undefined,
        AFT_STORAGE_DIR: undefined,
        AFT_BINARY_PATH: undefined,
        PATH: "",
        HOME: home,
        // CI runners export XDG_CONFIG_HOME, which would point config reads
        // at the runner's real config instead of this test's.
        XDG_CONFIG_HOME: join(tempDir, "config"),
        XDG_STATE_HOME: join(tempDir, "state"),
        XDG_DATA_HOME: join(tempDir, "data"),
        XDG_CACHE_HOME: cacheHome,
      });

      const versionDir = join(cacheHome, "aft", "bin", `v${PLUGIN_VERSION}`);
      mkdirSync(versionDir, { recursive: true });
      cachedAft = join(versionDir, "aft");
      writeFileSync(
        cachedAft,
        `#!/bin/sh\nprintf '%s\\n' "exec $*" >> ${JSON.stringify(execLog)}\necho "aft ${PLUGIN_VERSION}"\n`,
      );
      chmodSync(cachedAft, 0o755);

      mkdirSync(home, { recursive: true });
      writeUserConfig({ lsp: { auto_install: false }, semantic_search: false });
      prevCwd = process.cwd();
      process.chdir(home);
    });

    afterEach(() => {
      mock.restore();
      process.chdir(prevCwd);
      releaseEnv?.();
      releaseEnv = undefined;
      rmSync(tempDir, { recursive: true, force: true });
    });

    function writeUserConfig(config: Record<string, unknown>): void {
      const dir = join(tempDir, "config", "cortexkit");
      mkdirSync(dir, { recursive: true });
      writeFileSync(join(dir, "aft.jsonc"), JSON.stringify(config));
    }

    async function loadPlugin(): Promise<PiPlugin> {
      const mod = await import(`../index.js?no-binary-exec-${Date.now()}-${Math.random()}`);
      return mod.default;
    }

    function makePi(): Parameters<PiPlugin>[0] {
      return {
        registerTool: () => {},
        registerCommand: () => {},
        on: () => {},
      } as Parameters<PiPlugin>[0];
    }

    test("a cached binary with a matching identity sidecar is used without running it", async () => {
      writeBinaryIdentitySidecar(cachedAft, PLUGIN_VERSION, "0".repeat(64));
      const tools: string[] = [];
      const pi = makePi();
      (pi as { registerTool: (tool: { name: string }) => void }).registerTool = (tool) => {
        tools.push(tool.name);
      };

      await (await loadPlugin())(pi);

      // Tools registered means init resolved a binary and finished.
      expect(tools.length).toBeGreaterThan(0);
      // Pi's eager warmup may legitimately spawn the bridge process (the
      // stub logs that as an exec with no arguments): whether it is skipped
      // depends on the host's home directory, which Bun resolves from the
      // real account on Linux regardless of this test's HOME. What must never
      // happen during init is a version probe of the binary.
      const execs = existsSync(execLog) ? readFileSync(execLog, "utf8") : "";
      expect(execs.split("\n").filter((line) => line.trim() !== "exec" && line !== "")).toEqual([]);
    });

    test("subc mode resolves no local binary at all", async () => {
      writeUserConfig({
        lsp: { auto_install: false },
        semantic_search: false,
        subc: { connection_file: join(tempDir, "subc-connection.json") },
      });
      const resolverCalls: string[] = [];
      spyOn(bridge, "findBinarySync").mockImplementation(() => {
        resolverCalls.push("findBinarySync");
        return null;
      });
      spyOn(bridge, "findBinary").mockImplementation(async () => {
        resolverCalls.push("findBinary");
        return cachedAft;
      });
      spyOn(bridge, "ensureBinary").mockImplementation(async () => {
        resolverCalls.push("ensureBinary");
        return cachedAft;
      });
      let poolBinaryPath: string | null | undefined;
      let poolSubcFile: string | undefined;
      spyOn(bridge, "createAftTransportPool").mockImplementation(async (options) => {
        poolBinaryPath = options.binaryPath;
        poolSubcFile = options.subcConnectionFile;
        throw new Error("stop after the transport choice");
      });

      await expect((await loadPlugin())(makePi())).rejects.toThrow(
        "stop after the transport choice",
      );

      expect(poolSubcFile).toBe(join(tempDir, "subc-connection.json"));
      expect(poolBinaryPath).toBeNull();
      expect(resolverCalls).toEqual([]);
      expect(existsSync(execLog)).toBe(false);
    });
  },
);
