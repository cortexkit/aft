/// <reference path="../bun-test.d.ts" />

import { afterEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { acquireEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import type { HarnessAdapter, HarnessConfigPaths } from "../adapters/types.js";
import { printLspDoctorHelp, renderLspInspection, runLspDoctor } from "../commands/lsp.js";
import type { AftRequest, AftResponse } from "../lib/aft-bridge.js";

function makeAdapter(): HarnessAdapter {
  const configPaths: HarnessConfigPaths = {
    configDir: "/tmp/aft-test",
    harnessConfig: "/tmp/aft-test/opencode.jsonc",
    harnessConfigFormat: "jsonc",
    aftConfig: "/tmp/aft-test/aft.jsonc",
    aftConfigFormat: "jsonc",
  };

  return {
    kind: "opencode",
    displayName: "OpenCode",
    pluginPackageName: "@cortexkit/aft-opencode",
    pluginEntryWithVersion: "@cortexkit/aft-opencode@latest",
    isInstalled: () => true,
    getHostVersion: () => "test",
    detectConfigPaths: () => configPaths,
    hasPluginEntry: () => true,
    ensurePluginEntry: async () => ({
      ok: true,
      action: "already_present",
      message: "already registered",
      configPath: configPaths.harnessConfig,
    }),
    getPluginCacheInfo: () => ({
      path: "/tmp/aft-test/plugin-cache",
      exists: false,
    }),
    getStorageDir: () => "/tmp/aft-test/storage",
    getLogFile: () => "/tmp/aft-test/aft.log",
    getInstallHint: () => "Install OpenCode",
    clearPluginCache: async () => ({
      action: "not_found",
      path: "/tmp/aft-test/plugin-cache",
    }),
  };
}

function captureConsole(fn: () => void): string[] {
  const original = console.log;
  const lines: string[] = [];
  console.log = (message?: unknown) => {
    lines.push(String(message ?? ""));
  };
  try {
    fn();
  } finally {
    console.log = original;
  }
  return lines;
}

const tempRoots = new Set<string>();
const originalCwd = process.cwd();
let releaseEnv: (() => void) | undefined;

function tempRoot(prefix: string): string {
  const root = mkdtempSync(join(tmpdir(), prefix));
  tempRoots.add(root);
  return root;
}

afterEach(() => {
  process.chdir(originalCwd);
  releaseEnv?.();
  releaseEnv = undefined;
  for (const root of tempRoots) rmSync(root, { recursive: true, force: true });
  tempRoots.clear();
});

describe("doctor lsp", () => {
  test("renders help text when no file is passed", () => {
    const lines = captureConsole(() => printLspDoctorHelp());
    expect(lines.join("\n")).toContain("Usage: aft doctor lsp <file>");
  });

  test("passes --harness through adapter selection", async () => {
    const seenArgv: string[][] = [];
    const code = await runLspDoctor({
      argv: ["./sample.py", "--harness", "pi"],
      findBinary: () => "/tmp/aft",
      resolveAdapters: async (argv) => {
        seenArgv.push(argv);
        return [makeAdapter()];
      },
      sendRequest: async () => ({
        id: "doctor-lsp-inspect",
        success: true,
        file: "/tmp/sample.py",
        extension: "py",
        project_root: "/tmp",
        matching_servers: [],
        diagnostics_count: 0,
        diagnostics: [],
      }),
    });

    expect(code).toBe(0);
    expect(seenArgv[0]).toEqual(["./sample.py", "--harness", "pi"]);
  });

  test("renders structured lsp_inspect response as human text", () => {
    const output = renderLspInspection("./main.py", {
      success: true,
      file: "/repo/main.py",
      extension: "py",
      project_root: "/repo",
      experimental_lsp_ty: true,
      disabled_lsp: ["python"],
      lsp_paths_extra: ["/cache/bin"],
      matching_servers: [
        {
          id: "ty",
          name: "ty",
          kind: "ty",
          extensions: ["py", "pyi"],
          root_markers: ["requirements.txt"],
          binary_name: "ty",
          binary_path: "/usr/local/bin/ty",
          binary_source: "path",
          workspace_root: "/repo",
          spawn_status: "ok",
          args: ["server"],
        },
      ],
      diagnostics_count: 1,
      diagnostics: [
        {
          file: "/repo/main.py",
          line: 12,
          column: 8,
          severity: "error",
          message: "Undefined name 'foo'",
        },
      ],
    });

    expect(output).toContain("LSP inspection — ./main.py");
    expect(output).toContain("✓ ty");
    expect(output).toContain("Binary: /usr/local/bin/ty (found via path)");
    expect(output).toContain("/repo/main.py:12:8 [error] Undefined name 'foo'");
  });

  test("sends configure then lsp_inspect with the selected binary", async () => {
    const requests: AftRequest[][] = [];
    const responses: AftResponse[] = [
      { id: "doctor-lsp-configure", success: true },
      {
        id: "doctor-lsp-inspect",
        success: true,
        file: "/repo/main.py",
        extension: "py",
        project_root: "/repo",
        matching_servers: [
          {
            id: "ty",
            name: "ty",
            kind: "ty",
            extensions: ["py"],
            root_markers: ["requirements.txt"],
            binary_name: "ty",
            binary_path: null,
            binary_source: "not_found",
            workspace_root: "/repo",
            spawn_status: "binary_not_installed",
            args: ["server"],
          },
        ],
        diagnostics_count: 0,
        diagnostics: [],
      },
    ];
    const code = await runLspDoctor({
      argv: ["./main.py", "--harness", "opencode"],
      findBinary: () => "/tmp/aft-bin",
      resolveAdapters: async () => [makeAdapter()],
      sendRequests: async (binary, batch) => {
        expect(binary).toBe("/tmp/aft-bin");
        requests.push(batch);
        return responses;
      },
    });

    expect(code).toBe(0);
    expect(requests[0][0].command).toBe("configure");
    expect(requests[0][1].command).toBe("lsp_inspect");
  });

  test("infers cached lsp_paths_extra even when lsp.auto_install is false", async () => {
    const cacheRoot = tempRoot("aft-lsp-cache-");
    releaseEnv = await acquireEnv({ AFT_CACHE_DIR: cacheRoot });
    const cachedBinDir = join(cacheRoot, "lsp-packages", "pyright", "node_modules", ".bin");
    mkdirSync(cachedBinDir, { recursive: true });

    const projectRoot = tempRoot("aft-lsp-project-");
    mkdirSync(join(projectRoot, ".opencode"), { recursive: true });
    writeFileSync(
      join(projectRoot, ".opencode", "aft.json"),
      JSON.stringify({ lsp: { auto_install: false } }),
    );
    process.chdir(projectRoot);

    let configure: AftRequest | undefined;
    const code = await runLspDoctor({
      argv: ["./main.py", "--harness", "opencode"],
      findBinary: () => "/tmp/aft-bin",
      resolveAdapters: async () => [makeAdapter()],
      sendRequests: async (_binary, batch) => {
        configure = batch[0];
        return [
          { id: "doctor-lsp-configure", success: true },
          {
            id: "doctor-lsp-inspect",
            success: true,
            file: join(projectRoot, "main.py"),
            extension: "py",
            project_root: projectRoot,
            matching_servers: [],
            diagnostics_count: 0,
            diagnostics: [],
          },
        ];
      },
    });

    expect(code).toBe(0);
    expect(configure?.lsp_paths_extra).toContain(cachedBinDir);
  });
});
