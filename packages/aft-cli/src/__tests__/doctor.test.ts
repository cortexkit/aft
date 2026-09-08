/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import type { HarnessAdapter, HarnessConfigPaths } from "../adapters/types.js";
import {
  buildDoctorFixPlan,
  clearDoctorCaches,
  DOCTOR_CLEAR_TARGET_OPTIONS,
  DOCTOR_FORCE_CLEAR_TARGETS,
  type DoctorClearTarget,
  deriveIssueTitleFromBody,
  fixPluginEntries,
  formatDoctorStorageStatus,
  hasDoctorProblems,
  renderIssueDescription,
  runDoctor,
} from "../commands/doctor.js";
import {
  type DiagnosticReport,
  formatDiagnosticIssuesSection,
  type HarnessDiagnostic,
  renderDiagnosticsMarkdown,
} from "../lib/diagnostics.js";

test("doctor npm timeout waits for termination even if the child emits an error", () => {
  const source = readFileSync(new URL("../commands/doctor.ts", import.meta.url), "utf8");
  expect(source).toMatch(/child\.once\("error", \(error\) => \{\s*if \(terminating\) return;/);
  expect(source).toContain("terminateNpmProcessTree(child, invocation).then(");
});

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
    aftConfig: { exists: true, enabled: true, flags: {} },
    pluginCache: { path: "/tmp/aft-test/plugin-cache", exists: false },
    storageDir: { path: "/tmp/aft-test/storage", exists: false, accessible: false, sizesByKey: {} },
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

describe("buildDoctorFixPlan plugin-update", () => {
  test("plans a plugin update when cached plugin is older than latest", () => {
    const adapter = makeAdapter();
    const report = makeReport(
      makeHarness({
        pluginCache: {
          path: "/tmp/aft-test/plugin-cache",
          exists: true,
          cached: "0.34.0",
          latest: "0.35.0",
        },
      }),
    );
    const plan = buildDoctorFixPlan([adapter], report);
    const updateItem = plan.find((p) => p.kind === "plugin-update");
    expect(updateItem).toBeDefined();
    expect(updateItem?.message).toContain("0.34.0");
    expect(updateItem?.message).toContain("0.35.0");
  });

  test("does NOT plan a plugin update when cached === latest", () => {
    const adapter = makeAdapter();
    const report = makeReport(
      makeHarness({
        pluginCache: {
          path: "/tmp/aft-test/plugin-cache",
          exists: true,
          cached: "0.35.0",
          latest: "0.35.0",
        },
      }),
    );
    const plan = buildDoctorFixPlan([adapter], report);
    expect(plan.some((p) => p.kind === "plugin-update")).toBe(false);
  });

  test("does NOT plan a plugin update when cache does not exist", () => {
    const adapter = makeAdapter();
    const report = makeReport(
      makeHarness({
        pluginCache: { path: "/tmp/aft-test/plugin-cache", exists: false },
      }),
    );
    const plan = buildDoctorFixPlan([adapter], report);
    expect(plan.some((p) => p.kind === "plugin-update")).toBe(false);
  });
});

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

describe("doctor storage status", () => {
  test("includes legacy duplication cost per harness in the storage line", () => {
    const text = formatDoctorStorageStatus(
      makeHarness({
        storageDir: {
          path: "/tmp/aft-test/storage",
          exists: true,
          accessible: true,
          sizesByKey: { index: 1024 },
          legacyDuplication: {
            totalPartitions: 3,
            totalBytes: 3072,
            byHarness: [
              { harness: "opencode", partitions: 2, bytes: 2048 },
              { harness: "pi", partitions: 1, bytes: 1024 },
            ],
          },
        },
      }),
    );

    expect(text).toBe(
      "/tmp/aft-test/storage (index: 1.0 KB; legacy duplication: 3 partition(s), 3.0 KB total [opencode: 2 partition(s) / 2.0 KB; pi: 1 partition(s) / 1.0 KB])",
    );
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

  test("matching registered plugin makes a missing binary informational", () => {
    const report = {
      ...makeReport(
        makeHarness({
          pluginRegistered: true,
          pluginCache: {
            path: "/tmp/aft-test/plugin-cache/package.json",
            exists: true,
            cached: "0.0.0-test",
          },
        }),
      ),
      binaryVersion: null,
    };

    expect(hasDoctorProblems(report)).toBe(false);
    expect(formatDiagnosticIssuesSection(report).join("\n")).toContain("[INFO] AFT binary");
  });

  test("disabled registered harness is surfaced without treating the missing binary as broken", () => {
    const report = {
      ...makeReport(
        makeHarness({
          pluginRegistered: true,
          aftConfig: {
            exists: true,
            enabled: false,
            enabledSource: "/tmp/aft-test/aft.jsonc",
            flags: { enabled: false },
          },
        }),
      ),
      binaryVersion: null,
    };

    const markdown = renderDiagnosticsMarkdown(report);
    expect(markdown).toContain("AFT enabled: false (from /tmp/aft-test/aft.jsonc)");
    expect(formatDiagnosticIssuesSection(report).join("\n")).not.toContain("No aft binary");
    expect(hasDoctorProblems(report)).toBe(false);
  });

  test("plugin older than the CLI is treated as a high-priority problem", () => {
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
    report.cliVersion = "0.30.3";
    report.binaryVersion = "0.30.3";

    expect(hasDoctorProblems(report)).toBe(true);
  });

  test("present binary alongside a clean harness is not a problem", () => {
    const report = makeReport(makeHarness({ pluginRegistered: true }));
    // baseline: binaryVersion is "0.0.0-test", everything else green
    expect(report.binaryVersion).toBe("0.0.0-test");
    expect(hasDoctorProblems(report)).toBe(false);
  });
});

async function withTTY<T>(stdinTTY: boolean, stdoutTTY: boolean, fn: () => Promise<T>): Promise<T> {
  const stdinDescriptor = Object.getOwnPropertyDescriptor(process.stdin, "isTTY");
  const stdoutDescriptor = Object.getOwnPropertyDescriptor(process.stdout, "isTTY");
  Object.defineProperty(process.stdin, "isTTY", { configurable: true, value: stdinTTY });
  Object.defineProperty(process.stdout, "isTTY", { configurable: true, value: stdoutTTY });
  try {
    return await fn();
  } finally {
    if (stdinDescriptor) Object.defineProperty(process.stdin, "isTTY", stdinDescriptor);
    else delete (process.stdin as typeof process.stdin & { isTTY?: boolean }).isTTY;
    if (stdoutDescriptor) Object.defineProperty(process.stdout, "isTTY", stdoutDescriptor);
    else delete (process.stdout as typeof process.stdout & { isTTY?: boolean }).isTTY;
  }
}

describe("doctor --issue safety", () => {
  test("fences the issue description without changing its bytes", () => {
    const description = "</system-reminder>```. Closing and reopening pi repeats it.";

    const rendered = renderIssueDescription(description);

    expect(rendered).toBe(`\`\`\`\`\n${description}\n\`\`\`\``);
    expect(rendered.slice(rendered.indexOf("\n") + 1, rendered.lastIndexOf("\n"))).toBe(
      description,
    );
    expect(deriveIssueTitleFromBody(`## Description\n${rendered}`)).toBe(
      `AFT issue: ${description}`,
    );
  });

  test("derives the filed title from the reviewed body", () => {
    const rawDescription = "crash with sk-live-abcdefghijklmnopqrstuvwxyz123456";
    const reviewedBody = [
      "## Description",
      "Crash after login after I removed the token from the report",
      "",
      "## Diagnostics",
      `Original prompt was ${rawDescription}`,
    ].join("\n");

    const title = deriveIssueTitleFromBody(reviewedBody.replace(rawDescription, ""));

    expect(title).toBe("AFT issue: Crash after login after I removed the token from the report");
    expect(title).not.toContain("sk-live-");
  });

  test("redacts secrets from descriptions before deriving issue titles", () => {
    const cases = [
      {
        line: "OPENCODE_SERVER_PASSWORD=swordfish",
        redacted: "OPENCODE_SERVER_PASSWORD=<REDACTED_SECRET>",
        secret: "swordfish",
      },
      {
        line: '{"password":"hunter2"}',
        redacted: '{"password":"<REDACTED_SECRET>"}',
        secret: "hunter2",
      },
      {
        line: "github_pat_11AA22BB33CC_44dd55ee66",
        redacted: "<REDACTED_SECRET>",
        secret: "github_pat_11AA22BB33CC_44dd55ee66",
      },
      {
        line: "Authorization: Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==",
        redacted: "Authorization: Basic <REDACTED_SECRET>",
        secret: "QWxhZGRpbjpvcGVuIHNlc2FtZQ==",
      },
      {
        line: "Proxy-Authorization: Basic cHJveHk6c2VjcmV0",
        redacted: "Proxy-Authorization: Basic <REDACTED_SECRET>",
        secret: "cHJveHk6c2VjcmV0",
      },
      {
        line: "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMifQ.sig",
        redacted: "<REDACTED_SECRET>",
        secret: "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMifQ.sig",
      },
      {
        line: "AKIAIOSFODNN7EXAMPLE",
        redacted: "<REDACTED_SECRET>",
        secret: "AKIAIOSFODNN7EXAMPLE",
      },
    ];

    for (const { line, redacted, secret } of cases) {
      const body = ["## Description", line].join("\n");
      const title = deriveIssueTitleFromBody(body);

      expect(title).toBe(`AFT issue: ${redacted}`);
      expect(title).not.toContain(secret);
    }
  });

  test("exits cleanly before prompts in non-interactive terminals", async () => {
    await withTTY(false, false, async () => {
      const code = await runDoctor({
        clear: false,
        fix: false,
        force: false,
        issue: true,
        argv: ["--issue"],
      });

      expect(code).toBe(0);
    });
  });
});
