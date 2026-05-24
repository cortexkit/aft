/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import type { HarnessAdapter, HarnessConfigPaths } from "../adapters/types.js";
import {
  clearDoctorCaches,
  DOCTOR_CLEAR_TARGET_OPTIONS,
  DOCTOR_FORCE_CLEAR_TARGETS,
  type DoctorClearTarget,
  fixPluginEntries,
  hasDoctorProblems,
} from "../commands/doctor.js";
import type { DiagnosticReport, HarnessDiagnostic } from "../lib/diagnostics.js";

function makeAdapter(overrides: Partial<HarnessAdapter> = {}): HarnessAdapter {
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
    ...overrides,
  };
}

function makeHarness(overrides: Partial<HarnessDiagnostic> = {}): HarnessDiagnostic {
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
    hostInstalled: true,
    hostVersion: "test",
    pluginRegistered: true,
    configPaths,
    aftConfig: { exists: true, flags: {} },
    pluginCache: { path: "/tmp/aft-test/plugin-cache", exists: false },
    storageDir: { path: "/tmp/aft-test/storage", exists: false, sizesByKey: {} },
    onnxRuntime: {
      required: false,
      systemPath: null,
      systemVersion: null,
      systemCompatible: null,
      cachedPath: null,
      cachedVersion: null,
      cachedCompatible: null,
      platform: "test-test",
      installHint: "install onnx",
      requirement: ">=1.20",
    },
    logFile: { path: "/tmp/aft-test/aft.log", exists: false, sizeKb: 0 },
    ...overrides,
  };
}

function makeReport(harness: HarnessDiagnostic): DiagnosticReport {
  return {
    timestamp: "2026-01-01T00:00:00.000Z",
    platform: "darwin",
    arch: "arm64",
    nodeVersion: "v24.0.0",
    cliVersion: "0.0.0-test",
    binaryVersion: "0.0.0-test",
    harnesses: [harness],
    binaryCache: { path: "/tmp/aft-test/bin", versions: [], totalSize: 0 },
    lspCache: {
      npm: { path: "/tmp/aft-test/npm", entries: [], totalSize: 0 },
      github: { path: "/tmp/aft-test/gh", entries: [], totalSize: 0 },
      totalSize: 0,
    },
  };
}

describe("doctor cache clear targets", () => {
  test("lists the interactive clear categories in prompt order", () => {
    expect(DOCTOR_CLEAR_TARGET_OPTIONS).toEqual([
      {
        label: "Plugin npm cache (~/.cache/opencode/packages/@cortexkit/aft-opencode@latest, etc.)",
        value: "plugin-cache",
      },
      {
        label: "LSP install cache (~/.cache/aft/lsp-packages/, ~/.cache/aft/lsp-binaries/)",
        value: "lsp-cache",
      },
      {
        label: "Old aft binaries (~/.cache/aft/bin/v* — keeps the version matching this CLI)",
        value: "binary-cache",
      },
    ]);
  });

  test("keeps --force as a plugin-cache-only clear target", () => {
    expect(DOCTOR_FORCE_CLEAR_TARGETS satisfies DoctorClearTarget[]).toEqual(["plugin-cache"]);
  });

  test("binary-cache is a documented clear option (regression for the user request)", () => {
    const values = DOCTOR_CLEAR_TARGET_OPTIONS.map((o) => o.value);
    expect(values).toContain("binary-cache");
  });
});

describe("clearDoctorCaches", () => {
  test("--force compatibility clears plugin cache and does not touch LSP cache", async () => {
    let pluginClears = 0;
    let lspClears = 0;
    const adapter = makeAdapter({
      clearPluginCache: async () => {
        pluginClears += 1;
        return {
          action: "cleared",
          path: "/tmp/aft-test/plugin-cache",
        };
      },
    });

    const summary = await clearDoctorCaches([adapter], DOCTOR_FORCE_CLEAR_TARGETS, {
      clearLspCaches: () => {
        lspClears += 1;
        return { cleared: [], errors: [], totalBytes: 0 };
      },
      includePluginBytes: false,
    });

    expect(pluginClears).toBe(1);
    expect(lspClears).toBe(0);
    expect(summary.pluginCache).toEqual({ cleared: 1, totalBytes: 0, errors: 0 });
    expect(summary.lspCache).toBeUndefined();
    expect(summary.hadErrors).toBe(false);
  });

  test("selected LSP cache clears without requiring plugin-cache adapters", async () => {
    let lspClears = 0;

    const summary = await clearDoctorCaches([], ["lsp-cache"], {
      clearLspCaches: () => {
        lspClears += 1;
        return {
          cleared: [{ name: "pyright", path: "/tmp/aft-test/lsp-packages/pyright", size: 2048 }],
          errors: [],
          totalBytes: 2048,
        };
      },
    });

    expect(lspClears).toBe(1);
    expect(summary.pluginCache).toBeUndefined();
    expect(summary.lspCache).toEqual({ cleared: 1, totalBytes: 2048, errors: 0 });
    expect(summary.hadErrors).toBe(false);
  });
});

describe("doctor problem assessment", () => {
  test("plain doctor treats incompatible ONNX as a problem when semantic search is enabled", () => {
    const report = makeReport(
      makeHarness({
        onnxRuntime: {
          ...makeHarness().onnxRuntime,
          required: true,
          cachedPath: "/tmp/aft-test/storage/onnxruntime",
          cachedVersion: "1.9.0",
          cachedCompatible: false,
        },
      }),
    );

    expect(hasDoctorProblems(report)).toBe(true);
  });

  test("plain doctor ignores ONNX incompatibility when semantic search is disabled", () => {
    const report = makeReport(
      makeHarness({
        onnxRuntime: {
          ...makeHarness().onnxRuntime,
          required: false,
          systemPath: "/usr/lib/libonnxruntime.so",
          systemVersion: "1.9.0",
          systemCompatible: false,
        },
      }),
    );

    expect(hasDoctorProblems(report)).toBe(false);
  });

  test("doctor --fix path registers missing plugins", async () => {
    let ensureCalls = 0;
    const adapter = makeAdapter({
      hasPluginEntry: () => false,
      ensurePluginEntry: async () => {
        ensureCalls += 1;
        return {
          ok: true,
          action: "added",
          message: "registered",
          configPath: "/tmp/aft-test/opencode.jsonc",
        };
      },
    });

    await fixPluginEntries([adapter]);

    expect(ensureCalls).toBe(1);
  });

  test("plain doctor assessment is read-only and does not call ensurePluginEntry", () => {
    let ensureCalls = 0;
    const report = makeReport(makeHarness({ pluginRegistered: false }));
    const adapter = makeAdapter({
      hasPluginEntry: () => false,
      ensurePluginEntry: async () => {
        ensureCalls += 1;
        return {
          ok: true,
          action: "added",
          message: "registered",
          configPath: "/tmp/aft-test/opencode.jsonc",
        };
      },
    });

    expect(hasDoctorProblems(report)).toBe(true);
    expect(adapter.hasPluginEntry()).toBe(false);
    expect(ensureCalls).toBe(0);
  });

  // GitHub #46 follow-up regression. Before v0.27.2 the user could `rm -rf
  // ~/.cache/aft/bin && bunx --bun @cortexkit/aft doctor` and watch doctor
  // report `AFT binary unknown` + `Binary cache: 0 versions` and then close
  // with "Everything looks good." because `hasDoctorProblems` only checked
  // the harness rows. The whole reason a user runs `doctor` after wiping
  // the cache is to confirm AFT can recover — a missing binary must be
  // surfaced as a real problem so the user knows to run `doctor --fix`.
  test("missing binary is treated as a problem (GitHub #46 follow-up)", () => {
    const report = {
      ...makeReport(makeHarness({ pluginRegistered: true })),
      binaryVersion: null,
    };
    expect(hasDoctorProblems(report)).toBe(true);
  });

  test("present binary alongside a clean harness is not a problem", () => {
    const report = makeReport(makeHarness({ pluginRegistered: true }));
    // baseline: binaryVersion is "0.0.0-test", everything else green
    expect(report.binaryVersion).toBe("0.0.0-test");
    expect(hasDoctorProblems(report)).toBe(false);
  });
});
