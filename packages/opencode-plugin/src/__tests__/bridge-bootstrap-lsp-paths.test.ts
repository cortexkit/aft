/// <reference path="../bun-test.d.ts" />

/**
 * A GitHub-installed language server whose cache becomes ready while another
 * server's install lock is held by a different process must reach the
 * transport pool right away, not only after every install has settled (which
 * can take the full ten-minute lock wait). The pool copies `configOverrides`
 * when it is created, so changing that object alone reaches no bridge.
 */
import { describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { acquireEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import { startInstallLockOwner } from "../../../aft-bridge/src/__tests__/test-utils/github-layout.js";
import { writeStampedFileDigest } from "../../../aft-bridge/src/binary-identity.js";
import {
  type BridgeBootstrapDependencies,
  defaultBridgeBootstrapDependencies,
  prepareBridgeEnvironment,
} from "../bridge-bootstrap.js";
import type { AftConfig } from "../config.js";
import { abortInFlightGithubInstalls } from "../lsp-github-install.js";

const dependencies: BridgeBootstrapDependencies = {
  ...defaultBridgeBootstrapDependencies,
  resolveBinary: async () => "/never-spawned/aft",
  resolveStorageRoot: () => "/never-used/storage",
  buildConfigureParams: (_directory, state) => ({ ...state }),
  ensureOnnxRuntime: async () => null,
  isOrtAutoDownloadSupported: () => true,
  onnxInstallFailure: () => null,
};

/** Records what reaches the pool's own overrides and its live bridges. */
function recordingPool(initialOverrides: Record<string, unknown> = {}) {
  const overrides: Record<string, unknown> = { ...initialOverrides };
  const reconfigured: unknown[] = [];
  return {
    overrides,
    reconfigured,
    pool: {
      setConfigureOverride: (key: string, value: unknown) => {
        overrides[key] = value;
      },
      reconfigure: async (_projectRoot: string, params: Record<string, unknown>) => {
        reconfigured.push(params.lsp_paths_extra);
        return { attempted: 1, succeeded: 1, failed: 0 };
      },
    } as never,
  };
}

async function waitFor(condition: () => boolean, what: string): Promise<void> {
  const deadline = Date.now() + 5_000;
  while (!condition()) {
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
}

/**
 * A cache with texlab held under a real lock by another process and LuaLS
 * cached but not yet smoke-tested, so LuaLS validates in the background while
 * texlab keeps every install waiting.
 */
async function withHeldTexlabAndReadyLuaLs(
  action: (luaBinDir: string, texlabBinDir: string, lock: string) => Promise<void>,
): Promise<void> {
  const root = mkdtempSync(join(tmpdir(), "aft-bootstrap-lsp-paths-"));
  const releaseEnv = await acquireEnv({ AFT_CACHE_DIR: root });
  let owner: Awaited<ReturnType<typeof startInstallLockOwner>> | null = null;
  try {
    const script = "#!/bin/sh\nexit 0\n";
    const digest = createHash("sha256").update(script).digest("hex");

    const texlab = join(root, "lsp-binaries", "texlab", "bin", "texlab");
    mkdirSync(dirname(texlab), { recursive: true });
    writeFileSync(texlab, script, { mode: 0o755 });
    writeFileSync(
      join(root, "lsp-binaries", "texlab", ".aft-installed"),
      JSON.stringify({ version: "1.0.0", binarySha256: digest }),
    );

    const luaRoot = join(root, "lsp-binaries", "lua-ls", "extracted");
    const luaBinary = join(luaRoot, "bin", "lua-language-server");
    mkdirSync(dirname(luaBinary), { recursive: true });
    mkdirSync(join(luaRoot, "script"));
    writeFileSync(join(luaRoot, "main.lua"), "-- runtime\n");
    writeFileSync(luaBinary, script, { mode: 0o755 });
    writeStampedFileDigest(luaBinary, digest);
    writeFileSync(
      join(root, "lsp-binaries", "lua-ls", ".aft-installed"),
      JSON.stringify({
        version: "3.19.1",
        installedAt: "now",
        sha256: digest,
        binarySha256: digest,
        layout: "distribution",
      }),
    );

    const lock = join(
      root,
      "lsp-packages",
      encodeURIComponent("latex-lsp/texlab"),
      ".aft-installing",
    );
    owner = await startInstallLockOwner(lock);
    await action(dirname(luaBinary), dirname(texlab), lock);
  } finally {
    // Stop the background lock wait before removing its cache directory.
    await abortInFlightGithubInstalls();
    if (owner) {
      if (owner.child.exitCode === null) owner.child.stdin!.end("release");
      await owner.exited.catch(() => {});
    }
    releaseEnv();
    rmSync(root, { recursive: true, force: true });
  }
}

async function boot() {
  const project = mkdtempSync(join(tmpdir(), "aft-bootstrap-lsp-project-"));
  try {
    return await prepareBridgeEnvironment(
      {
        configRoot: project,
        lspDirectory: project,
        config: { lsp: { auto_install: false }, disabled_tools: [] } as unknown as AftConfig,
        pluginVersion: "0.0.0-test",
        notify: () => {},
      },
      dependencies,
    );
  } finally {
    rmSync(project, { recursive: true, force: true });
  }
}

const posixOnly = test.skipIf(process.platform === "win32");

describe("OpenCode per-server LSP path publication", () => {
  posixOnly(
    "a server validated while another is locked reaches the attached pool and its live bridges",
    async () => {
      await withHeldTexlabAndReadyLuaLs(async (luaBinDir, texlabBinDir, lock) => {
        const environment = await boot();
        const { overrides, reconfigured, pool } = recordingPool(environment.configOverrides);
        environment.attach(pool);

        await waitFor(
          () => ((overrides.lsp_paths_extra as string[] | undefined) ?? []).includes(luaBinDir),
          "LuaLS in the pool's overrides",
        );
        await waitFor(
          () => reconfigured.some((paths) => (paths as string[]).includes(luaBinDir)),
          "LuaLS pushed to live bridges",
        );
        // texlab is still locked by the other process, so installs have not
        // settled: this publication came from the per-server path.
        expect(existsSync(lock)).toBe(true);
        expect(overrides.lsp_paths_extra as string[]).not.toContain(texlabBinDir);
      });
    },
  );

  posixOnly(
    "a server validated before the pool exists is in the overrides the pool is created with",
    async () => {
      await withHeldTexlabAndReadyLuaLs(async (luaBinDir, texlabBinDir, lock) => {
        const environment = await boot();
        await waitFor(
          () =>
            ((environment.configOverrides.lsp_paths_extra as string[] | undefined) ?? []).includes(
              luaBinDir,
            ),
          "LuaLS in the overrides the pool is created from",
        );
        const { overrides, pool } = recordingPool(environment.configOverrides);
        expect(overrides.lsp_paths_extra as string[]).toContain(luaBinDir);
        environment.attach(pool);
        expect(existsSync(lock)).toBe(true);
        expect(overrides.lsp_paths_extra as string[]).not.toContain(texlabBinDir);
      });
    },
  );

  posixOnly(
    "a server validated after the pool copied its overrides but before attach is pushed at attach",
    async () => {
      await withHeldTexlabAndReadyLuaLs(async (luaBinDir, _texlabBinDir, lock) => {
        const environment = await boot();
        // The pool copies the overrides before LuaLS has validated, and
        // `environment.attach(pool)` below runs only after LuaLS has validated.
        const { overrides, reconfigured, pool } = recordingPool(environment.configOverrides);
        expect((overrides.lsp_paths_extra as string[] | undefined) ?? []).not.toContain(luaBinDir);
        await waitFor(
          () =>
            ((environment.configOverrides.lsp_paths_extra as string[] | undefined) ?? []).includes(
              luaBinDir,
            ),
          "LuaLS validation",
        );
        environment.attach(pool);
        await waitFor(
          () => ((overrides.lsp_paths_extra as string[] | undefined) ?? []).includes(luaBinDir),
          "LuaLS pushed at attach",
        );
        await waitFor(
          () => reconfigured.some((paths) => (paths as string[]).includes(luaBinDir)),
          "LuaLS pushed to live bridges at attach",
        );
        expect(existsSync(lock)).toBe(true);
      });
    },
  );
});
