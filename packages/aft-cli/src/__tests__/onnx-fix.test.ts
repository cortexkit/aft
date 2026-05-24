/// <reference path="../bun-test.d.ts" />

/**
 * Tests for `runOnnxFix` and `findOnnxFixCandidates`.
 *
 * Scope: pure logic tests against synthetic `DiagnosticReport` shapes.
 * We don't actually delete `~/.local/share/opencode/storage/plugin/aft/`
 * — `runOnnxFix` accepts injected `confirmFn` and `rmFn` for that exact
 * reason.
 *
 * The cases here are the user-facing ones. Each describes a state the
 * TUI sidebar would render and asserts the fix flow does the right
 * thing — no clearing, clearing the AFT-managed cache, or refusing
 * because the user declined.
 */

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { DiagnosticReport, HarnessDiagnostic } from "../lib/diagnostics.js";
import { findOnnxFixCandidates, runOnnxFix } from "../lib/onnx-fix.js";

let workDir: string;

beforeEach(() => {
  workDir = mkdtempSync(join(tmpdir(), "aft-onnx-fix-"));
});

afterEach(() => {
  rmSync(workDir, { recursive: true, force: true });
});

function makeHarness(overrides: Partial<HarnessDiagnostic> = {}): HarnessDiagnostic {
  const storageDir = overrides.storageDir?.path ?? join(workDir, "storage");
  return {
    kind: "opencode",
    displayName: "OpenCode",
    hostInstalled: true,
    hostVersion: "test",
    pluginRegistered: true,
    configPaths: {
      configDir: workDir,
      harnessConfig: join(workDir, "opencode.jsonc"),
      harnessConfigFormat: "jsonc",
      aftConfig: join(workDir, "aft.jsonc"),
      aftConfigFormat: "jsonc",
    },
    aftConfig: { exists: true, flags: {} },
    pluginCache: { path: "/tmp/plugin-cache", exists: false },
    storageDir: {
      path: storageDir,
      exists: true,
      sizesByKey: {},
    },
    onnxRuntime: {
      required: true,
      systemPath: null,
      systemVersion: null,
      systemCompatible: null,
      cachedPath: null,
      cachedVersion: null,
      cachedCompatible: null,
      platform: "linux-x64",
      installHint: "AFT auto-downloads",
      requirement: ">=1.20",
    },
    logFile: { path: "/tmp/aft.log", exists: false, sizeKb: 0 },
    ...overrides,
  };
}

function makeReport(harnesses: HarnessDiagnostic[]): DiagnosticReport {
  return {
    timestamp: "2026-05-06T00:00:00Z",
    platform: "linux",
    arch: "x64",
    nodeVersion: "v24.0.0",
    cliVersion: "0.19.5",
    binaryVersion: "0.19.5",
    harnesses,
    binaryCache: { versions: [], activeVersion: null, totalSize: 0, path: workDir },
    lspCache: {
      npm: { entries: [], path: join(workDir, "lsp-packages"), totalSize: 0 },
      github: { entries: [], path: join(workDir, "lsp-binaries"), totalSize: 0 },
      totalSize: 0,
    },
  };
}

describe("findOnnxFixCandidates", () => {
  test("returns empty when ONNX is not required", () => {
    const report = makeReport([
      makeHarness({
        onnxRuntime: {
          ...makeHarness().onnxRuntime,
          required: false,
        },
      }),
    ]);
    expect(findOnnxFixCandidates(report)).toEqual([]);
  });

  test("returns empty when ONNX is healthy (compatible cached, no system)", () => {
    const report = makeReport([
      makeHarness({
        onnxRuntime: {
          ...makeHarness().onnxRuntime,
          cachedPath: join(workDir, "storage/onnxruntime/1.24.4"),
          cachedVersion: "1.24.4",
          cachedCompatible: true,
        },
      }),
    ]);
    expect(findOnnxFixCandidates(report)).toEqual([]);
  });

  test("flags incompatible CACHED install for re-download", () => {
    // Someone's plugin downloaded an old AFT-managed ONNX (e.g. before
    // we bumped to v1.24). Clearing the cache forces a fresh download.
    const storagePath = join(workDir, "storage");
    mkdirSync(join(storagePath, "onnxruntime"), { recursive: true });
    const report = makeReport([
      makeHarness({
        storageDir: { path: storagePath, exists: true, sizesByKey: {} },
        onnxRuntime: {
          ...makeHarness().onnxRuntime,
          cachedPath: join(storagePath, "onnxruntime/1.18.0"),
          cachedVersion: "1.18.0",
          cachedCompatible: false,
        },
      }),
    ]);
    const candidates = findOnnxFixCandidates(report);
    expect(candidates).toHaveLength(1);
    expect(candidates[0].reason).toContain("cached ONNX Runtime");
    expect(candidates[0].reason).toContain("1.18.0");
    expect(candidates[0].storageOnnxDir).toBe(join(storagePath, "onnxruntime"));
  });

  test("flags incompatible SYSTEM install (the screenshot case)", () => {
    const storagePath = join(workDir, "storage");
    mkdirSync(storagePath, { recursive: true });
    const report = makeReport([
      makeHarness({
        storageDir: { path: storagePath, exists: true, sizesByKey: {} },
        onnxRuntime: {
          ...makeHarness().onnxRuntime,
          systemPath: "/usr/lib/x86_64-linux-gnu",
          systemVersion: "1.9.0",
          systemCompatible: false,
        },
      }),
    ]);
    const candidates = findOnnxFixCandidates(report);
    expect(candidates).toHaveLength(1);
    expect(candidates[0].reason).toContain("system ONNX Runtime");
    expect(candidates[0].reason).toContain("1.9.0");
    expect(candidates[0].reason).toContain("v0.19.5+ skips incompatible system installs");
  });

  test("does NOT flag system install when a compatible cached install exists", () => {
    // The cached install will win; the system one is harmless.
    const report = makeReport([
      makeHarness({
        onnxRuntime: {
          ...makeHarness().onnxRuntime,
          systemPath: "/usr/lib/x86_64-linux-gnu",
          systemVersion: "1.9.0",
          systemCompatible: false,
          cachedPath: join(workDir, "storage/onnxruntime/1.24.4"),
          cachedVersion: "1.24.4",
          cachedCompatible: true,
        },
      }),
    ]);
    expect(findOnnxFixCandidates(report)).toEqual([]);
  });
});

describe("runOnnxFix", () => {
  test("returns null when no fixable issues exist (healthy state)", async () => {
    const report = makeReport([
      makeHarness({
        onnxRuntime: {
          ...makeHarness().onnxRuntime,
          cachedPath: join(workDir, "storage/onnxruntime/1.24.4"),
          cachedVersion: "1.24.4",
          cachedCompatible: true,
        },
      }),
    ]);
    const result = await runOnnxFix([], report, { yes: true });
    expect(result).toBe(null);
  });

  test("returns null when user declines the prompt (no destructive action)", async () => {
    const storagePath = join(workDir, "storage");
    const onnxDir = join(storagePath, "onnxruntime");
    mkdirSync(onnxDir, { recursive: true });

    const report = makeReport([
      makeHarness({
        storageDir: { path: storagePath, exists: true, sizesByKey: {} },
        onnxRuntime: {
          ...makeHarness().onnxRuntime,
          cachedPath: join(onnxDir, "1.18.0"),
          cachedVersion: "1.18.0",
          cachedCompatible: false,
        },
      }),
    ]);

    let rmCalls = 0;
    const result = await runOnnxFix([], report, {
      confirmFn: async () => false, // user says no
      rmFn: () => {
        rmCalls += 1;
      },
    });

    expect(result).toBe(null);
    expect(rmCalls).toBe(0); // critical: must not delete on decline
  });

  test("clears the storage onnxruntime dir on consent", async () => {
    const storagePath = join(workDir, "storage");
    const onnxDir = join(storagePath, "onnxruntime");
    mkdirSync(join(onnxDir, "1.18.0"), { recursive: true });

    const report = makeReport([
      makeHarness({
        storageDir: { path: storagePath, exists: true, sizesByKey: {} },
        onnxRuntime: {
          ...makeHarness().onnxRuntime,
          cachedPath: join(onnxDir, "1.18.0"),
          cachedVersion: "1.18.0",
          cachedCompatible: false,
        },
      }),
    ]);

    const removed: string[] = [];
    const result = await runOnnxFix([], report, {
      confirmFn: async () => true,
      rmFn: (path) => {
        removed.push(path);
      },
    });

    expect(result).not.toBe(null);
    expect(result?.cleared).toBe(1);
    expect(result?.errors).toEqual([]);
    // Critical safety: the path deleted must be inside our test workDir,
    // never `/usr/lib/...`.
    expect(removed).toHaveLength(1);
    expect(removed[0]).toBe(onnxDir);
    expect(removed[0]).toContain(workDir);
  });

  test("never targets system paths even when the issue is a system install", async () => {
    const storagePath = join(workDir, "storage");
    mkdirSync(storagePath, { recursive: true });

    const report = makeReport([
      makeHarness({
        storageDir: { path: storagePath, exists: true, sizesByKey: {} },
        onnxRuntime: {
          ...makeHarness().onnxRuntime,
          systemPath: "/usr/lib/x86_64-linux-gnu",
          systemVersion: "1.9.0",
          systemCompatible: false,
        },
      }),
    ]);

    const removed: string[] = [];
    await runOnnxFix([], report, {
      confirmFn: async () => true,
      rmFn: (path) => removed.push(path),
    });

    // The candidate's storage dir doesn't exist (we never downloaded
    // an AFT-managed ONNX), so nothing should be deleted. The flow
    // must NOT try to remove `/usr/lib/...`.
    for (const path of removed) {
      expect(path).not.toContain("/usr/lib");
      expect(path).not.toContain("/opt/homebrew");
      expect(path).toContain(workDir);
    }
  });
});
