/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { appendFileSync, mkdtempSync, truncateSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  collectDiagnosticIssues,
  type DiagnosticReport,
  findPluginCliVersionSkews,
  formatDiagnosticIssuesSection,
  type HarnessDiagnostic,
  renderDiagnosticsMarkdown,
  tailLogFile,
} from "../lib/diagnostics.js";

describe("tailLogFile", () => {
  test("tails a large log from the end", () => {
    const dir = mkdtempSync(join(tmpdir(), "aft-cli-tail-test-"));
    const path = join(dir, "large.log");
    writeFileSync(path, "start\n");
    truncateSync(path, 100 * 1024 * 1024);
    appendFileSync(path, "line-1\nline-2\nline-3\n");

    expect(tailLogFile(path, 2)).toBe("line-2\nline-3");
  });
});

function makeHarness(overrides: Partial<HarnessDiagnostic> = {}): HarnessDiagnostic {
  return {
    kind: "opencode",
    displayName: "OpenCode",
    hostInstalled: true,
    hostVersion: "test",
    pluginRegistered: true,
    configPaths: {
      configDir: "/tmp/aft-test",
      harnessConfig: "/tmp/aft-test/opencode.jsonc",
      harnessConfigFormat: "jsonc",
      aftConfig: "/tmp/aft-test/aft.jsonc",
      aftConfigFormat: "jsonc",
    },
    aftConfig: { exists: true, enabled: true, flags: {} },
    pluginCache: { path: "/tmp/aft-test/plugin-cache", exists: false },
    storageDir: { path: "/tmp/aft-test/storage", exists: true, accessible: true, sizesByKey: {} },
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
      autoDownloadable: true,
      requirement: ">=1.20",
    },
    logFile: { path: "/tmp/aft-test/aft.log", exists: false, sizeKb: 0 },
    ...overrides,
  };
}

function makeReport(harness: HarnessDiagnostic): DiagnosticReport {
  return {
    timestamp: "2026-01-01T00:00:00.000Z",
    platform: "win32",
    arch: "x64",
    nodeVersion: "v24.0.0",
    cliVersion: "0.30.3",
    binaryVersion: "0.30.3",
    harnesses: [harness],
    binaryCache: { path: "/tmp/aft-test/bin", versions: [], totalSize: 0 },
    lspCache: {
      npm: { path: "/tmp/aft-test/npm", entries: [], totalSize: 0 },
      github: { path: "/tmp/aft-test/gh", entries: [], totalSize: 0 },
      totalSize: 0,
    },
  };
}

describe("missing ONNX Runtime remediation", () => {
  const missingOnnx = (
    overrides: Partial<HarnessDiagnostic["onnxRuntime"]>,
  ): string | undefined => {
    const report = makeReport(
      makeHarness({
        onnxRuntime: { ...makeHarness().onnxRuntime, required: true, ...overrides },
      }),
    );
    return collectDiagnosticIssues(report).find((issue) => issue.code === "onnx_missing")
      ?.remediation;
  };

  // Apple Silicon is an auto-download platform, and the advice used to read
  // "install ONNX Runtime manually (AFT auto-downloads ONNX Runtime on Apple
  // Silicon)", which tells the user to do by hand the thing it says is
  // automatic.
  test("sends a platform AFT downloads for to doctor --fix and nowhere else", () => {
    const remediation = missingOnnx({
      autoDownloadable: true,
      platform: "darwin-arm64",
      installHint: "AFT auto-downloads ONNX Runtime on Apple Silicon",
    });

    expect(remediation).toBe(
      "Run `npx @cortexkit/aft doctor --fix` to download the AFT-managed ONNX Runtime.",
    );
    expect(remediation).not.toContain("manual");
  });

  // Microsoft publishes no macOS x64 build, so there is nothing for --fix to
  // fetch and Homebrew really is the route.
  test("keeps the manual route where there is no published build to download", () => {
    const remediation = missingOnnx({
      autoDownloadable: false,
      platform: "darwin-x64",
      installHint: "brew install onnxruntime (Intel Mac — no published build)",
    });

    expect(remediation).toContain("brew install onnxruntime");
    expect(remediation).toContain("darwin-x64");
    expect(remediation).not.toContain("doctor --fix");
  });
});

describe("diagnostic issue summaries", () => {
  test("reports plugin/CLI version skew as a high-severity issue", () => {
    const report = makeReport(
      makeHarness({
        pluginCache: {
          path: "/tmp/aft-test/plugin-cache",
          exists: true,
          cached: "0.29.1",
          latest: "0.30.3",
        },
      }),
    );

    const issues = collectDiagnosticIssues(report);
    const skews = findPluginCliVersionSkews(report);
    const section = formatDiagnosticIssuesSection(report).join("\n");

    expect(skews).toHaveLength(1);
    expect(skews[0]).toMatchObject({
      code: "plugin_cli_version_skew",
      severity: "high",
      scope: "OpenCode",
    });
    expect(section).toContain("--- Issues found ---");
    expect(section).toContain(
      "Plugin version (0.29.1) is older than CLI (0.30.3). New binary cache won't be used until you update the plugin.",
    );
    expect(section).toContain(
      "Remediation: Update `@cortexkit/aft-opencode` in your harness config to `@latest`.",
    );
    expect(section.includes(String.fromCharCode(27))).toBe(false);
    expect(issues.some((issue) => issue.code === "plugin_cli_version_skew")).toBe(true);
  });

  test("skips plugin/CLI skew when the plugin package is not installed", () => {
    const report = makeReport(
      makeHarness({ pluginCache: { path: "/tmp/missing", exists: false } }),
    );

    expect(findPluginCliVersionSkews(report)).toHaveLength(0);
  });

  test("downgrades a missing binary to INFO when a matching plugin can install it", () => {
    const report = makeReport(
      makeHarness({
        pluginCache: {
          path: "/tmp/pi/npm/node_modules/@cortexkit/aft-pi/package.json",
          exists: true,
          cached: "0.30.3",
        },
      }),
    );
    report.binaryVersion = null;

    const issue = collectDiagnosticIssues(report).find((entry) => entry.code === "binary_missing");

    expect(issue).toMatchObject({
      severity: "info",
      message:
        "No aft binary matching CLI 0.30.3 was detected; it will self-install when the next AFT-enabled session starts.",
    });
  });

  test("keeps a missing binary HIGH when no plugin session can install it", () => {
    const report = makeReport(makeHarness({ pluginRegistered: false }));
    report.binaryVersion = null;

    expect(collectDiagnosticIssues(report)).toContainEqual(
      expect.objectContaining({ code: "binary_missing", severity: "high" }),
    );
  });

  test("doctor --issue markdown includes the issue summary and plugin version", () => {
    const report = makeReport(
      makeHarness({
        pluginCache: {
          path: "/tmp/aft-test/plugin-cache",
          exists: true,
          cached: "0.29.1",
          latest: "0.30.3",
        },
      }),
    );

    const markdown = renderDiagnosticsMarkdown(report);

    expect(markdown).toContain("### Issues found");
    expect(markdown).toContain(
      "**HIGH** OpenCode: Plugin version (0.29.1) is older than CLI (0.30.3)",
    );
    expect(markdown).toContain("- Plugin version: 0.29.1");
  });

  test("storage markdown includes legacy duplication summary when present", () => {
    const markdown = renderDiagnosticsMarkdown(
      makeReport(
        makeHarness({
          storageDir: {
            path: "/tmp/aft-test/storage",
            exists: true,
            accessible: true,
            sizesByKey: { index: 1024 },
            legacyDuplication: {
              totalPartitions: 2,
              totalBytes: 2048,
              byHarness: [{ harness: "opencode", partitions: 2, bytes: 2048 }],
            },
          },
        }),
      ),
    );

    expect(markdown).toContain('"legacyDuplication"');
    expect(markdown).toContain('"totalPartitions": 2');
    expect(markdown).toContain('"harness": "opencode"');
  });
});
