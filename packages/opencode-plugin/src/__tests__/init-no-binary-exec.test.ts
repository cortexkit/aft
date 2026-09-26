/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
/**
 * Plugin init must never execute the `aft` binary on the host thread.
 *
 * OpenCode runs every plugin on one JavaScript thread. Asking a cached binary
 * for `--version` blocked that thread inside `posix_spawn` until the child was
 * loaded, which froze the host for up to a minute per instance boot when the
 * 85 MB binary had gone cold. These tests boot the shared bridge bootstrap
 * with its real binary resolver against a cached stub `aft` that records every
 * time it is executed, and require that it was never run.
 */
import { linkCachedExecutable } from "../../../aft-bridge/src/__tests__/test-utils/cached-executable.js";
import { acquireEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import { writeBinaryIdentitySidecar } from "../../../aft-bridge/src/binary-identity.js";
import {
  type BridgeBootstrapDependencies,
  defaultBridgeBootstrapDependencies,
  prepareBridgeEnvironment,
} from "../bridge-bootstrap.js";
import type { AftConfig } from "../config.js";

const VERSION = "1.2.3";
const posixOnly = process.platform === "win32";

describe.skipIf(posixOnly)("OpenCode plugin init never executes the aft binary", () => {
  let root: string;
  let execLog: string;
  let cachedAft: string;
  let releaseEnv: (() => void) | undefined;

  beforeEach(async () => {
    root = mkdtempSync(join(tmpdir(), "aft-opencode-no-exec-"));
    execLog = join(root, "exec.log");
    const cacheDir = join(root, "cache");
    cachedAft = join(cacheDir, "bin", `v${VERSION}`, "aft");
    mkdirSync(join(cacheDir, "bin", `v${VERSION}`), { recursive: true });
    // Records every execution, whatever the arguments, then answers like aft.
    linkCachedExecutable(
      cachedAft,
      `#!/bin/sh\nprintf '%s\\n' "exec $*" >> "$AFT_TEST_EXEC_LOG"\necho "aft ${VERSION}"\n`,
    );
    releaseEnv = await acquireEnv({
      AFT_BINARY_PATH: undefined,
      AFT_CACHE_DIR: cacheDir,
      HOME: join(root, "home"),
      PATH: "",
      AFT_TEST_EXEC_LOG: execLog,
    });
  });

  afterEach(() => {
    releaseEnv?.();
    releaseEnv = undefined;
    rmSync(root, { recursive: true, force: true });
  });

  function dependencies(
    overrides: Partial<BridgeBootstrapDependencies> = {},
  ): BridgeBootstrapDependencies {
    return {
      ...defaultBridgeBootstrapDependencies,
      ensureStorageMigrated: async () => {},
      resolveStorageRoot: () => join(root, "storage"),
      buildConfigureParams: (_directory, state) => ({ ...state }),
      ensureOnnxRuntime: async () => null,
      startLspAutoInstall: () => null,
      ...overrides,
    };
  }

  function boot(config: AftConfig, deps: BridgeBootstrapDependencies) {
    return prepareBridgeEnvironment(
      {
        configRoot: root,
        lspDirectory: root,
        config,
        pluginVersion: VERSION,
        notify: () => {},
      },
      deps,
    );
  }

  test("a cached binary with a matching identity sidecar is used without running it", async () => {
    writeBinaryIdentitySidecar(cachedAft, VERSION, "0".repeat(64));

    const environment = await boot({} as AftConfig, dependencies());

    expect(environment.binaryPath).toBe(cachedAft);
    expect(existsSync(execLog) ? readFileSync(execLog, "utf8") : "").toBe("");
  });

  test("subc mode resolves no local binary at all", async () => {
    let resolved = false;
    const environment = await boot(
      { subc: { connection_file: join(root, "subc-connection.json") } } as AftConfig,
      dependencies({
        resolveBinary: async () => {
          resolved = true;
          return cachedAft;
        },
      }),
    );

    expect(resolved).toBe(false);
    expect(environment.binaryPath).toBeNull();
    expect(existsSync(execLog)).toBe(false);
  });
});
