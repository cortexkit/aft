import { afterEach, beforeEach, describe, expect, mock, test } from "bun:test";
import { existsSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";

import { OpenCodeAdapter } from "../../../../aft-cli/src/adapters/opencode.js";
import { cacheDir, getOpenCodeCacheRoot } from "./constants.js";

const logMock = mock(() => {});
const warnMock = mock(() => {});

const checkerMocks = {
  extractChannel: mock(() => "latest"),
  findPluginEntry: mock(() => null),
  getCachedVersion: mock(() => null),
  getCurrentRuntimePackageJsonPath: mock(() => null),
  getLatestVersion: mock(async () => null),
  getLocalDevVersion: mock(() => null),
};

const cacheMocks = {
  preparePackageUpdate: mock(() => "/tmp/opencode"),
  resolveInstallContext: mock(() => ({ installDir: "/tmp/opencode" })),
  runNpmInstallSafe: mock(async () => ({ ok: true })),
};

mock.module("../../logger.js", () => ({
  log: logMock,
  debug: mock(() => {}),
  warn: warnMock,
  error: mock(() => {}),
}));

mock.module("./checker.js", () => checkerMocks);
mock.module("./cache.js", () => cacheMocks);

let importCounter = 0;

function freshIndexImport() {
  return import(`./index.ts?test=${importCounter++}`);
}

function createCtx() {
  const showToast = mock(() => Promise.resolve(undefined));
  return {
    ctx: {
      directory: "/test",
      client: { tui: { showToast } },
    },
    showToast,
  };
}

async function waitForCalls(fn: { mock: { calls: unknown[] } }, minCalls = 1): Promise<void> {
  const deadline = Date.now() + 1000;

  while (fn.mock.calls.length < minCalls) {
    if (Date.now() > deadline) throw new Error("Timed out waiting for async hook work");
    await new Promise((resolve) => setTimeout(resolve, 0));
  }
}

let testStorageDir: string;

describe("auto-update-checker/index", () => {
  test("CLI and auto-update cache roots stay aligned", () => {
    // All three resolve at call time under the same environment, so this holds
    // regardless of what XDG_* mutations other test files performed earlier.
    expect(dirname(cacheDir())).toBe(getOpenCodeCacheRoot());
    expect(new OpenCodeAdapter().getOpenCodeCacheDir()).toBe(dirname(cacheDir()));
  });
  beforeEach(() => {
    testStorageDir = mkdtempSync(join(tmpdir(), "aft-update-test-"));
    logMock.mockClear();
    warnMock.mockClear();

    checkerMocks.extractChannel.mockReset();
    checkerMocks.extractChannel.mockImplementation(() => "latest");
    checkerMocks.findPluginEntry.mockReset();
    checkerMocks.findPluginEntry.mockImplementation(() => null);
    checkerMocks.getCachedVersion.mockReset();
    checkerMocks.getCachedVersion.mockImplementation(() => null);
    checkerMocks.getCurrentRuntimePackageJsonPath.mockReset();
    checkerMocks.getCurrentRuntimePackageJsonPath.mockImplementation(() => null);
    checkerMocks.getLatestVersion.mockReset();
    checkerMocks.getLatestVersion.mockImplementation(async () => null);
    checkerMocks.getLocalDevVersion.mockReset();
    checkerMocks.getLocalDevVersion.mockImplementation(() => null);

    cacheMocks.preparePackageUpdate.mockReset();
    cacheMocks.preparePackageUpdate.mockImplementation(() => "/tmp/opencode");
    cacheMocks.resolveInstallContext.mockReset();
    cacheMocks.resolveInstallContext.mockImplementation(() => ({ installDir: "/tmp/opencode" }));
    cacheMocks.runNpmInstallSafe.mockReset();
    cacheMocks.runNpmInstallSafe.mockImplementation(async () => ({ ok: true }));
  });

  afterEach(() => {
    mock.restore();
  });

  test("uses resolved install root for auto-update installs", async () => {
    const { getAutoUpdateInstallDir } = await freshIndexImport();

    expect(getAutoUpdateInstallDir()).toBe("/tmp/opencode");
  });

  test("shows development toast and skips background update for local dev installs", async () => {
    checkerMocks.getLocalDevVersion.mockImplementation(() => "0.17.2-dev");
    const { createAutoUpdateCheckerHook } = await freshIndexImport();
    const { ctx, showToast } = createCtx();

    createAutoUpdateCheckerHook(ctx as Parameters<typeof createAutoUpdateCheckerHook>[0], {
      storageDir: testStorageDir,
      initDelayMs: 0,
    });
    await waitForCalls(showToast);

    expect(showToast).toHaveBeenCalledWith({
      body: {
        title: "AFT 0.17.2-dev (dev)",
        message: "Running in local development mode.",
        variant: "info",
        duration: 3000,
      },
    });
    expect(checkerMocks.findPluginEntry).not.toHaveBeenCalled();
    expect(checkerMocks.getLatestVersion).not.toHaveBeenCalled();
  });

  test("event hook is a no-op (does not trigger duplicate checks)", async () => {
    checkerMocks.getLocalDevVersion.mockImplementation(() => "0.17.2-dev");
    const { createAutoUpdateCheckerHook } = await freshIndexImport();
    const { ctx, showToast } = createCtx();

    const hook = createAutoUpdateCheckerHook(
      ctx as Parameters<typeof createAutoUpdateCheckerHook>[0],
      { storageDir: testStorageDir, initDelayMs: 0 },
    );
    await waitForCalls(showToast);
    expect(showToast).toHaveBeenCalledTimes(1);

    // Firing events afterwards should not trigger any additional checks.
    await hook({ event: { type: "session.created", properties: { info: {} } } });
    await hook({ event: { type: "session.idle", properties: { info: {} } } });
    await new Promise((resolve) => setTimeout(resolve, 20));

    expect(showToast).toHaveBeenCalledTimes(1);
  });

  test("disabled hook never schedules a check", async () => {
    checkerMocks.getLocalDevVersion.mockImplementation(() => "0.17.2-dev");
    const { createAutoUpdateCheckerHook } = await freshIndexImport();
    const { ctx, showToast } = createCtx();

    createAutoUpdateCheckerHook(ctx as Parameters<typeof createAutoUpdateCheckerHook>[0], {
      enabled: false,
      storageDir: testStorageDir,
      initDelayMs: 0,
    });
    await new Promise((resolve) => setTimeout(resolve, 30));

    expect(showToast).not.toHaveBeenCalled();
  });

  test("on-disk timestamp dedupes concurrent plugin instances", async () => {
    checkerMocks.getLocalDevVersion.mockImplementation(() => "0.17.2-dev");
    const { createAutoUpdateCheckerHook } = await freshIndexImport();
    const { ctx: ctx1, showToast: showToast1 } = createCtx();
    const { ctx: ctx2, showToast: showToast2 } = createCtx();

    // First instance claims the slot and runs the check.
    createAutoUpdateCheckerHook(ctx1 as Parameters<typeof createAutoUpdateCheckerHook>[0], {
      storageDir: testStorageDir,
      initDelayMs: 0,
    });
    await waitForCalls(showToast1);
    expect(showToast1).toHaveBeenCalledTimes(1);

    // Second instance, same storageDir, fired immediately afterwards
    // sees the recent timestamp and skips its check entirely.
    createAutoUpdateCheckerHook(ctx2 as Parameters<typeof createAutoUpdateCheckerHook>[0], {
      storageDir: testStorageDir,
      initDelayMs: 0,
    });
    await new Promise((resolve) => setTimeout(resolve, 30));

    expect(showToast2).not.toHaveBeenCalled();
  });

  test("expired timestamp allows a new check to run", async () => {
    checkerMocks.getLocalDevVersion.mockImplementation(() => "0.17.2-dev");
    const { createAutoUpdateCheckerHook } = await freshIndexImport();
    const { ctx, showToast } = createCtx();

    // Pre-write a stale timestamp (2 hours ago) into the dedup file.
    const stale = Date.now() - 2 * 60 * 60 * 1000;
    writeFileSync(
      join(testStorageDir, "last-update-check.json"),
      JSON.stringify({ lastCheckedMs: stale }),
      "utf-8",
    );

    createAutoUpdateCheckerHook(ctx as Parameters<typeof createAutoUpdateCheckerHook>[0], {
      storageDir: testStorageDir,
      checkIntervalMs: 60 * 60 * 1000, // 1h interval
      initDelayMs: 0,
    });
    await waitForCalls(showToast);

    expect(showToast).toHaveBeenCalledTimes(1);

    // Verify the timestamp file was updated to a recent value.
    const after = JSON.parse(
      readFileSync(join(testStorageDir, "opencode", "last-update-check.json"), "utf-8"),
    ) as { lastCheckedMs: number };
    expect(after.lastCheckedMs).toBeGreaterThan(stale);
  });

  test("repairs root-scoped timestamp into opencode harness path", async () => {
    checkerMocks.getLocalDevVersion.mockImplementation(() => "0.17.2-dev");
    const { createAutoUpdateCheckerHook } = await freshIndexImport();
    const { ctx, showToast } = createCtx();
    const recent = Date.now();
    writeFileSync(
      join(testStorageDir, "last-update-check.json"),
      JSON.stringify({ lastCheckedMs: recent }),
      "utf-8",
    );

    createAutoUpdateCheckerHook(ctx as Parameters<typeof createAutoUpdateCheckerHook>[0], {
      storageDir: testStorageDir,
      checkIntervalMs: 60 * 60 * 1000,
      initDelayMs: 0,
    });
    await new Promise((resolve) => setTimeout(resolve, 30));

    expect(showToast).not.toHaveBeenCalled();
    expect(existsSync(join(testStorageDir, "last-update-check.json"))).toBe(false);
    const repaired = JSON.parse(
      readFileSync(join(testStorageDir, "opencode", "last-update-check.json"), "utf-8"),
    ) as { lastCheckedMs: number };
    expect(repaired.lastCheckedMs).toBe(recent);
  });

  test("shows success toast after updating the active install root", async () => {
    checkerMocks.findPluginEntry.mockImplementation(() => ({
      entry: "@cortexkit/aft-opencode@latest",
      pinnedVersion: null,
      isPinned: false,
      configPath: "/config/opencode.jsonc",
    }));
    checkerMocks.getCachedVersion.mockImplementation(() => "0.17.1");
    checkerMocks.getLatestVersion.mockImplementation(async () => "0.17.2");

    const { createAutoUpdateCheckerHook } = await freshIndexImport();
    const { ctx, showToast } = createCtx();
    createAutoUpdateCheckerHook(ctx as Parameters<typeof createAutoUpdateCheckerHook>[0], {
      showStartupToast: false,
      storageDir: testStorageDir,
      initDelayMs: 0,
    });

    await waitForCalls(showToast);

    expect(cacheMocks.preparePackageUpdate).toHaveBeenCalledWith(
      "0.17.2",
      "@cortexkit/aft-opencode",
    );
    expect(cacheMocks.runNpmInstallSafe).toHaveBeenCalledWith(
      "/tmp/opencode",
      expect.objectContaining({ signal: expect.any(AbortSignal) }),
    );
    expect(showToast).toHaveBeenCalledWith({
      body: {
        title: "AFT Updated!",
        message: "v0.17.1 → v0.17.2\nRestart OpenCode to apply.",
        variant: "success",
        duration: 8000,
      },
    });
  });

  test("shows notification-only toast when auto-update is disabled", async () => {
    checkerMocks.findPluginEntry.mockImplementation(() => ({
      entry: "@cortexkit/aft-opencode@latest",
      pinnedVersion: null,
      isPinned: false,
      configPath: "/config/opencode.jsonc",
    }));
    checkerMocks.getCachedVersion.mockImplementation(() => "0.17.1");
    checkerMocks.getLatestVersion.mockImplementation(async () => "0.17.2");
    const { createAutoUpdateCheckerHook } = await freshIndexImport();
    const { ctx, showToast } = createCtx();

    createAutoUpdateCheckerHook(ctx as Parameters<typeof createAutoUpdateCheckerHook>[0], {
      showStartupToast: false,
      autoUpdate: false,
      storageDir: testStorageDir,
      initDelayMs: 0,
    });
    await waitForCalls(showToast);

    expect(showToast).toHaveBeenCalledWith({
      body: {
        title: "AFT 0.17.2",
        message: "v0.17.2 available. Auto-update is disabled.",
        variant: "info",
        duration: 8000,
      },
    });
    expect(cacheMocks.preparePackageUpdate).not.toHaveBeenCalled();
    expect(cacheMocks.runNpmInstallSafe).not.toHaveBeenCalled();
  });

  test("shows pinned-version notification without installing", async () => {
    checkerMocks.findPluginEntry.mockImplementation(() => ({
      entry: "@cortexkit/aft-opencode@0.17.1",
      pinnedVersion: "0.17.1",
      isPinned: true,
      configPath: "/config/opencode.jsonc",
    }));
    checkerMocks.getCachedVersion.mockImplementation(() => "0.17.1");
    checkerMocks.getLatestVersion.mockImplementation(async () => "0.17.2");
    const { createAutoUpdateCheckerHook } = await freshIndexImport();
    const { ctx, showToast } = createCtx();

    createAutoUpdateCheckerHook(ctx as Parameters<typeof createAutoUpdateCheckerHook>[0], {
      showStartupToast: false,
      storageDir: testStorageDir,
      initDelayMs: 0,
    });
    await waitForCalls(showToast);

    expect(showToast).toHaveBeenCalledWith({
      body: {
        title: "AFT 0.17.2",
        message:
          "v0.17.2 available. Version is pinned; update your OpenCode plugin config to upgrade.",
        variant: "info",
        duration: 8000,
      },
    });
    expect(cacheMocks.preparePackageUpdate).not.toHaveBeenCalled();
  });

  test("shows warning toast when latest version fetch fails", async () => {
    checkerMocks.findPluginEntry.mockImplementation(() => ({
      entry: "@cortexkit/aft-opencode@latest",
      pinnedVersion: null,
      isPinned: false,
      configPath: "/config/opencode.jsonc",
    }));
    checkerMocks.getCachedVersion.mockImplementation(() => "0.17.1");
    checkerMocks.getLatestVersion.mockImplementation(async () => null);
    const { createAutoUpdateCheckerHook } = await freshIndexImport();
    const { ctx, showToast } = createCtx();

    createAutoUpdateCheckerHook(ctx as Parameters<typeof createAutoUpdateCheckerHook>[0], {
      showStartupToast: false,
      storageDir: testStorageDir,
      initDelayMs: 0,
    });
    await waitForCalls(showToast);

    expect(showToast).toHaveBeenCalledWith({
      body: {
        title: "AFT update check failed",
        message:
          "Could not check npm for @cortexkit/aft-opencode updates. Continuing with the cached version.",
        variant: "warning",
        duration: 8000,
      },
    });
  });

  // The drill log showed "Update available (latest): 0.58.0 → 0.57.2": the
  // installed release was ahead of the dist-tag and the checker offered the
  // older one. Neither case may toast, prepare, or install anything.
  for (const [label, installed, latest] of [
    ["installed is newer than latest", "0.58.0", "0.57.2"],
    ["installed equals latest", "0.58.0", "0.58.0"],
    ["installed release is newer than a latest prerelease", "0.58.0", "0.58.0-beta.1"],
  ] as const) {
    test(`offers no update when ${label}`, async () => {
      checkerMocks.findPluginEntry.mockImplementation(() => ({
        entry: "@cortexkit/aft-opencode@latest",
        pinnedVersion: null,
        isPinned: false,
        configPath: "/config/opencode.jsonc",
      }));
      checkerMocks.getCachedVersion.mockImplementation(() => installed);
      checkerMocks.getLatestVersion.mockImplementation(async () => latest);
      const { createAutoUpdateCheckerHook } = await freshIndexImport();
      const { ctx, showToast } = createCtx();

      createAutoUpdateCheckerHook(ctx as Parameters<typeof createAutoUpdateCheckerHook>[0], {
        showStartupToast: false,
        storageDir: testStorageDir,
        initDelayMs: 0,
      });
      await waitForCalls(checkerMocks.getLatestVersion);
      await waitForCalls(logMock);
      await new Promise((resolve) => setTimeout(resolve, 20));

      const logged = logMock.mock.calls.map((call) => String((call as unknown[])[0]));
      expect(logged.some((line) => line.includes("Already on latest version"))).toBe(true);
      expect(logged.some((line) => line.includes("Update available"))).toBe(false);
      expect(showToast).not.toHaveBeenCalled();
      expect(cacheMocks.preparePackageUpdate).not.toHaveBeenCalled();
      expect(cacheMocks.runNpmInstallSafe).not.toHaveBeenCalled();
    });
  }

  test("isNewerVersion orders by semver with prerelease", async () => {
    const { isNewerVersion } = await freshIndexImport();
    expect(isNewerVersion("0.57.2", "0.58.0")).toBe(false);
    expect(isNewerVersion("0.58.0", "0.58.0")).toBe(false);
    expect(isNewerVersion("0.58.1", "0.58.0")).toBe(true);
    expect(isNewerVersion("0.10.0", "0.9.9")).toBe(true);
    expect(isNewerVersion("0.58.0", "0.58.0-beta.2")).toBe(true);
    expect(isNewerVersion("0.58.0-beta.2", "0.58.0")).toBe(false);
    expect(isNewerVersion("0.58.0-beta.10", "0.58.0-beta.2")).toBe(true);
    expect(isNewerVersion("not-a-version", "0.58.0")).toBe(false);
  });

  test("shows install failure toast without telling users to restart", async () => {
    checkerMocks.findPluginEntry.mockImplementation(() => ({
      entry: "@cortexkit/aft-opencode@latest",
      pinnedVersion: null,
      isPinned: false,
      configPath: "/config/opencode.jsonc",
    }));
    checkerMocks.getCachedVersion.mockImplementation(() => "0.17.1");
    checkerMocks.getLatestVersion.mockImplementation(async () => "0.17.2");
    cacheMocks.runNpmInstallSafe.mockImplementation(async () => ({
      ok: false,
      reason: "npm install exited with code 1",
      stderrTail: "MODULE_NOT_FOUND\n",
    }));
    const { createAutoUpdateCheckerHook } = await freshIndexImport();
    const { ctx, showToast } = createCtx();

    createAutoUpdateCheckerHook(ctx as Parameters<typeof createAutoUpdateCheckerHook>[0], {
      showStartupToast: false,
      storageDir: testStorageDir,
      initDelayMs: 0,
    });
    await waitForCalls(showToast);

    expect(showToast).toHaveBeenCalledWith({
      body: {
        title: "AFT 0.17.2",
        message:
          "v0.17.2 available, but auto-update failed to install it. Check logs or retry manually.",
        variant: "error",
        duration: 8000,
      },
    });
  });
});
