import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  unlinkSync,
  utimesSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { acquireEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import { getAftCacheRoot, getAftLspPackagesDir } from "../../../aft-bridge/src/cache-paths.js";
import {
  acquireInstallLock,
  aftCacheBase,
  claimLspAutoInstallPass,
  isInstalled,
  lspBinaryPath,
  lspBinDir,
  lspCacheRoot,
  lspPackageDir,
  readVersionCheck,
  shouldRecheckVersion,
  withInstallLock,
  writeVersionCheck,
} from "../lsp-cache";

let tempCache: string;
let releaseEnv: (() => void) | undefined;

beforeEach(async () => {
  tempCache = mkdtempSync(join(tmpdir(), "aft-lsp-cache-test-"));
  releaseEnv = await acquireEnv({
    AFT_CACHE_DIR: tempCache,
    LOCALAPPDATA: process.env.LOCALAPPDATA,
    APPDATA: process.env.APPDATA,
    XDG_CACHE_HOME: process.env.XDG_CACHE_HOME,
  });
});

afterEach(() => {
  releaseEnv?.();
  releaseEnv = undefined;
  rmSync(tempCache, { recursive: true, force: true });
});

function withPlatform<T>(platform: NodeJS.Platform, fn: () => T): T {
  const descriptor = Object.getOwnPropertyDescriptor(process, "platform");
  Object.defineProperty(process, "platform", { configurable: true, value: platform });
  try {
    return fn();
  } finally {
    if (descriptor) Object.defineProperty(process, "platform", descriptor);
  }
}

describe("lsp-cache layout", () => {
  test("delegates cache paths to aft-bridge", () => {
    expect(aftCacheBase()).toBe(getAftCacheRoot());
    expect(lspCacheRoot()).toBe(getAftLspPackagesDir());
  });

  test("lspCacheRoot honors AFT_CACHE_DIR", () => {
    expect(lspCacheRoot()).toBe(join(tempCache, "lsp-packages"));
  });

  test("lspCacheRoot uses LOCALAPPDATA on Windows when no override is set", () => {
    delete process.env.AFT_CACHE_DIR;
    process.env.LOCALAPPDATA = join(tempCache, "LocalAppData");
    delete process.env.APPDATA;

    withPlatform("win32", () => {
      expect(lspCacheRoot()).toBe(join(process.env.LOCALAPPDATA as string, "aft", "lsp-packages"));
    });
  });

  test("lspPackageDir url-encodes scoped packages", () => {
    const dir = lspPackageDir("@vue/language-server");
    expect(dir).toContain(encodeURIComponent("@vue/language-server"));
    expect(dir.startsWith(lspCacheRoot())).toBe(true);
  });

  test("lspBinaryPath joins package dir with node_modules/.bin/<binary>", () => {
    const path = lspBinaryPath("typescript-language-server", "typescript-language-server");
    expect(path).toContain("node_modules");
    expect(path.endsWith(join(".bin", "typescript-language-server"))).toBe(true);
  });

  test("lspBinDir returns parent of binary path", () => {
    const dir = lspBinDir("typescript-language-server");
    expect(dir.endsWith(join("node_modules", ".bin"))).toBe(true);
  });

  test("isInstalled returns false when binary doesn't exist", () => {
    expect(isInstalled("nonexistent-pkg", "nonexistent-bin")).toBe(false);
  });

  test("isInstalled returns true after the binary file is created", () => {
    const pkg = "fake-pkg";
    const bin = "fake-bin";
    const path = lspBinaryPath(pkg, bin);
    mkdirSync(join(path, ".."), { recursive: true });
    writeFileSync(path, "#!/bin/sh\nexit 0\n");
    expect(isInstalled(pkg, bin)).toBe(true);
  });

  test("isInstalled finds a Windows .cmd shim", () => {
    const pkg = "fake-win-pkg";
    const bin = "fake-win-bin";
    const path = `${lspBinaryPath(pkg, bin)}.cmd`;
    mkdirSync(join(path, ".."), { recursive: true });
    writeFileSync(path, "@echo off\r\n");

    withPlatform("win32", () => {
      expect(isInstalled(pkg, bin)).toBe(true);
    });
  });
});

describe("install lock", () => {
  test("first acquire succeeds, second fails while held", () => {
    const lease = acquireInstallLock("pkg-a");
    expect(lease).not.toBeNull();
    expect(acquireInstallLock("pkg-a")).toBeNull();
    lease?.release();
  });

  test("after release, acquire succeeds again", () => {
    acquireInstallLock("pkg-b")?.release();
    const lease = acquireInstallLock("pkg-b");
    expect(lease).not.toBeNull();
    lease?.release();
  });

  test("retains the cross-process lock until stale recovery when requested", async () => {
    expect(
      await withInstallLock("pkg-retained", async (lease) => {
        lease.retain();
        return true;
      }),
    ).toBe(true);

    const lockFile = join(lspPackageDir("pkg-retained"), ".aft-installing");
    expect(existsSync(lockFile)).toBe(true);
    expect(acquireInstallLock("pkg-retained")).toBeNull();
    unlinkSync(lockFile);
  });

  test("locks for different packages are independent", () => {
    const first = acquireInstallLock("pkg-c");
    const second = acquireInstallLock("pkg-d");
    expect(first).not.toBeNull();
    expect(second).not.toBeNull();
    first?.release();
    second?.release();
  });

  test("an older same-process lease cannot release a reclaimed generation", () => {
    const first = acquireInstallLock("pkg-generation");
    expect(first).not.toBeNull();
    const lockFile = join(lspPackageDir("pkg-generation"), ".aft-installing");
    const stale = new Date(Date.now() - 31 * 60 * 1000);
    utimesSync(lockFile, stale, stale);

    const second = acquireInstallLock("pkg-generation");
    expect(second).not.toBeNull();
    first?.release();
    expect(existsSync(lockFile)).toBe(true);
    second?.release();
    expect(existsSync(lockFile)).toBe(false);
  });

  // TOCTOU defense — a process must not delete another
  // process's lock during its own release. Simulates the scenario where
  // process A's lock was reclaimed by B (because A exceeded STALE_LOCK_MS)
  // and A then runs releaseInstallLock in its finally block.
  test("releaseInstallLock leaves a lock owned by a different PID intact", () => {
    // Acquire as ourselves.
    const lease = acquireInstallLock("pkg-toctou");
    const lockFile = join(lspPackageDir("pkg-toctou"), ".aft-installing");

    // Simulate "another process owns this lock" by overwriting the PID.
    const otherPid = process.pid + 99999;
    writeFileSync(lockFile, `${otherPid}\n${new Date().toISOString()}\nother-token\n`);

    // We must NOT delete it.
    lease?.release();
    expect(existsSync(lockFile)).toBe(true);
    const raw = readFileSync(lockFile, "utf8");
    expect(raw.startsWith(String(otherPid))).toBe(true);

    // Cleanup.
    unlinkSync(lockFile);
  });
});

describe("auto-install pass slot", () => {
  test("two plugin instances get exactly one auto-install pass in one process", () => {
    const first = claimLspAutoInstallPass();
    const second = claimLspAutoInstallPass();
    expect([first, second].filter(Boolean)).toHaveLength(1);
    first?.release();
    second?.release();
  });

  test("two node processes racing one cache get exactly one auto-install pass", async () => {
    const bundleDir = join(tempCache, "bundle");
    mkdirSync(bundleDir, { recursive: true });
    const build = await Bun.build({
      entrypoints: [fileURLToPath(new URL("../lsp-cache.ts", import.meta.url))],
      outdir: bundleDir,
      naming: "lsp-cache.mjs",
      target: "node",
      format: "esm",
    });
    expect(build.success).toBe(true);

    const bundlePath = join(bundleDir, "lsp-cache.mjs");
    const resultPath = join(tempCache, "race-results.txt");
    const helperPath = fileURLToPath(
      new URL("./fixtures/lsp-autoinstall-race-helper.mjs", import.meta.url),
    );
    const env = { ...process.env, AFT_CACHE_DIR: join(tempCache, "shared-race-cache") };
    const children = [
      Bun.spawn(["node", helperPath, bundlePath, resultPath], { env }),
      Bun.spawn(["node", helperPath, bundlePath, resultPath], { env }),
    ];
    expect(await Promise.all(children.map((child) => child.exited))).toEqual([0, 0]);

    const outcomes = readFileSync(resultPath, "utf8").trim().split("\n").sort();
    expect(outcomes).toEqual(["skipped", "winner"]);
  }, 15_000);
});

describe("version-check record", () => {
  test("readVersionCheck returns null when file is absent", () => {
    expect(readVersionCheck("absent-pkg")).toBeNull();
  });

  test("write then read round-trips the latest_eligible field", () => {
    writeVersionCheck("pkg-x", "1.2.3");
    const record = readVersionCheck("pkg-x");
    expect(record?.latest_eligible).toBe("1.2.3");
    expect(record?.last_checked).toMatch(/^\d{4}-\d{2}-\d{2}T/);
  });

  test("write with null latest_eligible round-trips", () => {
    writeVersionCheck("pkg-y", null);
    const record = readVersionCheck("pkg-y");
    expect(record?.latest_eligible).toBeNull();
  });

  test("readVersionCheck returns null when file is malformed JSON", () => {
    const dir = lspPackageDir("pkg-z");
    mkdirSync(dir, { recursive: true });
    writeFileSync(join(dir, ".aft-version-check"), "not valid json {");
    expect(readVersionCheck("pkg-z")).toBeNull();
  });

  test("readVersionCheck returns null when last_checked is missing", () => {
    const dir = lspPackageDir("pkg-q");
    mkdirSync(dir, { recursive: true });
    writeFileSync(join(dir, ".aft-version-check"), JSON.stringify({ latest_eligible: "1.0.0" }));
    expect(readVersionCheck("pkg-q")).toBeNull();
  });

  test("shouldRecheckVersion: null record always recheckable", () => {
    expect(shouldRecheckVersion(null)).toBe(true);
  });

  test("shouldRecheckVersion: fresh record skipped, old record re-checked", () => {
    const now = Date.now();
    const fresh = {
      last_checked: new Date(now - 1000).toISOString(),
      latest_eligible: "1.0.0",
    };
    const stale = {
      last_checked: new Date(now - 8 * 24 * 60 * 60 * 1000).toISOString(),
      latest_eligible: "1.0.0",
    };
    expect(shouldRecheckVersion(fresh)).toBe(false);
    expect(shouldRecheckVersion(stale)).toBe(true);
  });

  test("shouldRecheckVersion: malformed last_checked treated as recheckable", () => {
    const broken = { last_checked: "not a date", latest_eligible: "1.0.0" };
    expect(shouldRecheckVersion(broken)).toBe(true);
  });
});

describe("cache directory creation", () => {
  test("acquireInstallLock creates the package dir if missing", () => {
    expect(existsSync(lspPackageDir("created-by-lock"))).toBe(false);
    const lease = acquireInstallLock("created-by-lock");
    expect(existsSync(lspPackageDir("created-by-lock"))).toBe(true);
    lease?.release();
  });
});
