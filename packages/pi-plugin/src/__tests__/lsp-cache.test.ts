import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { acquireEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import {
  acquireInstallLock,
  isInstalled,
  lspBinaryPath,
  lspBinDir,
  lspCacheRoot,
  lspPackageDir,
  readVersionCheck,
  releaseInstallLock,
  shouldRecheckVersion,
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
    expect(acquireInstallLock("pkg-a")).toBe(true);
    expect(acquireInstallLock("pkg-a")).toBe(false);
    releaseInstallLock("pkg-a");
  });

  test("after release, acquire succeeds again", () => {
    acquireInstallLock("pkg-b");
    releaseInstallLock("pkg-b");
    expect(acquireInstallLock("pkg-b")).toBe(true);
    releaseInstallLock("pkg-b");
  });

  test("releaseInstallLock on a non-existent lock is safe", () => {
    expect(() => releaseInstallLock("never-acquired")).not.toThrow();
  });

  test("locks for different packages are independent", () => {
    expect(acquireInstallLock("pkg-c")).toBe(true);
    expect(acquireInstallLock("pkg-d")).toBe(true);
    releaseInstallLock("pkg-c");
    releaseInstallLock("pkg-d");
  });
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
    acquireInstallLock("created-by-lock");
    expect(existsSync(lspPackageDir("created-by-lock"))).toBe(true);
    releaseInstallLock("created-by-lock");
  });
});
