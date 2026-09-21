/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  __test__ as bridgeOnnxTest,
  isOrtAutoDownloadSupported,
} from "../../../aft-bridge/src/onnx-runtime.js";
import {
  findCachedOnnxRuntime,
  findIgnoredWindowsSystemOnnxRuntime,
  findSystemOnnxRuntime,
  getManualInstallHint,
  getOnnxLibraryName,
  ONNX_RUNTIME_VERSION,
} from "../lib/onnx.js";

type EnvSnapshot = Map<string, string | undefined>;

let workDir: string;
let envSnapshot: EnvSnapshot;

beforeEach(() => {
  workDir = mkdtempSync(join(tmpdir(), "aft-cli-onnx-test-"));
  envSnapshot = new Map([
    ["PATH", process.env.PATH],
    ["Path", process.env.Path],
    ["path", process.env.path],
    ["USERPROFILE", process.env.USERPROFILE],
    ["ProgramFiles", process.env.ProgramFiles],
    ["ProgramFiles(x86)", process.env["ProgramFiles(x86)"]],
    ["SystemRoot", process.env.SystemRoot],
    ["windir", process.env.windir],
  ]);
});

afterEach(() => {
  for (const [key, value] of envSnapshot) {
    if (value === undefined) delete process.env[key];
    else process.env[key] = value;
  }
  rmSync(workDir, { recursive: true, force: true });
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

function withArch<T>(arch: string, fn: () => T): T {
  const descriptor = Object.getOwnPropertyDescriptor(process, "arch");
  Object.defineProperty(process, "arch", { configurable: true, value: arch });
  try {
    return fn();
  } finally {
    if (descriptor) Object.defineProperty(process, "arch", descriptor);
  }
}

describe("CLI ONNX system detection", () => {
  test("doctor detects Windows ONNX Runtime from PATH", () => {
    const runtimeDir = join(workDir, "onnxruntime", "bin");
    mkdirSync(runtimeDir, { recursive: true });
    writeFileSync(join(runtimeDir, "OnNxRuNtImE.DlL"), "binary");
    process.env.PATH = `${join(workDir, "missing")};${runtimeDir}`;
    process.env.ProgramFiles = join(workDir, "program-files");
    process.env["ProgramFiles(x86)"] = join(workDir, "program-files-x86");
    process.env.USERPROFILE = join(workDir, "profile-without-nuget");
    delete process.env.Path;
    delete process.env.path;

    const found = withPlatform("win32", () => findSystemOnnxRuntime());

    expect(found).toBe(runtimeDir);
  });

  test("doctor rejects an unversioned ONNX Runtime from Windows System32", () => {
    const windowsDir = join(workDir, "Windows");
    const systemDir = join(windowsDir, "System32");
    mkdirSync(systemDir, { recursive: true });
    writeFileSync(join(systemDir, "onnxruntime.dll"), "Windows component");
    process.env.PATH = systemDir;
    process.env.SystemRoot = windowsDir;
    process.env.ProgramFiles = join(workDir, "program-files");
    process.env["ProgramFiles(x86)"] = join(workDir, "program-files-x86");
    process.env.USERPROFILE = join(workDir, "profile-without-nuget");
    delete process.env.Path;
    delete process.env.path;
    delete process.env.windir;

    const result = withPlatform("win32", () => ({
      found: findSystemOnnxRuntime(),
      ignored: findIgnoredWindowsSystemOnnxRuntime(),
    }));

    expect(result.found).toBeNull();
    expect(result.ignored).toBe(systemDir);
  });

  test("doctor skips incompatible Windows PATH DLLs and selects a compatible candidate", () => {
    const oldRuntimeDir = join(workDir, "onnxruntime-1.9.0", "bin");
    const newRuntimeDir = join(workDir, "onnxruntime-1.24.4", "bin");
    mkdirSync(oldRuntimeDir, { recursive: true });
    mkdirSync(newRuntimeDir, { recursive: true });
    writeFileSync(join(oldRuntimeDir, "onnxruntime.dll"), "old binary");
    writeFileSync(join(newRuntimeDir, "onnxruntime.dll"), "new binary");
    process.env.PATH = `${oldRuntimeDir};${newRuntimeDir}`;
    process.env.ProgramFiles = join(workDir, "program-files");
    process.env["ProgramFiles(x86)"] = join(workDir, "program-files-x86");
    process.env.USERPROFILE = join(workDir, "profile-without-nuget");
    delete process.env.Path;
    delete process.env.path;

    const found = withPlatform("win32", () => findSystemOnnxRuntime());

    expect(found).toBe(newRuntimeDir);
  });

  test("doctor treats a detected incompatible Windows PATH DLL as absent", () => {
    const oldRuntimeDir = join(workDir, "onnxruntime-1.9.0", "bin");
    mkdirSync(oldRuntimeDir, { recursive: true });
    writeFileSync(join(oldRuntimeDir, "onnxruntime.dll"), "old binary");
    process.env.PATH = oldRuntimeDir;
    process.env.ProgramFiles = join(workDir, "program-files");
    process.env["ProgramFiles(x86)"] = join(workDir, "program-files-x86");
    process.env.USERPROFILE = join(workDir, "profile-without-nuget");
    delete process.env.Path;
    delete process.env.path;

    const found = withPlatform("win32", () => findSystemOnnxRuntime());

    expect(found).toBeNull();
  });

  test("doctor scans Windows NuGet caches and skips incompatible versions", () => {
    const oldNativeDir = join(
      workDir,
      ".nuget",
      "packages",
      "microsoft.ml.onnxruntime",
      "1.9.0",
      "runtimes",
      "win-x64",
      "native",
    );
    const newNativeDir = join(
      workDir,
      ".nuget",
      "packages",
      "microsoft.ml.onnxruntime",
      "1.24.4",
      "runtimes",
      "win-x64",
      "native",
    );
    mkdirSync(oldNativeDir, { recursive: true });
    mkdirSync(newNativeDir, { recursive: true });
    writeFileSync(join(oldNativeDir, "onnxruntime.dll"), "old binary");
    writeFileSync(join(newNativeDir, "onnxruntime.dll"), "new binary");
    process.env.PATH = join(workDir, "missing");
    process.env.ProgramFiles = join(workDir, "program-files");
    process.env["ProgramFiles(x86)"] = join(workDir, "program-files-x86");
    process.env.USERPROFILE = workDir;
    delete process.env.Path;
    delete process.env.path;

    const found = withPlatform("win32", () => findSystemOnnxRuntime());

    expect(found).toBe(newNativeDir);
  });
});

describe("CLI ONNX cached detection (#71)", () => {
  test("finds the cached library in the version root", () => {
    const versionDir = join(workDir, "onnxruntime", ONNX_RUNTIME_VERSION);
    mkdirSync(versionDir, { recursive: true });
    writeFileSync(join(versionDir, getOnnxLibraryName()), "stub");
    expect(findCachedOnnxRuntime(workDir)).toBe(versionDir);
  });

  test("finds the cached library in a lib/ subdir (manual install)", () => {
    const versionDir = join(workDir, "onnxruntime", ONNX_RUNTIME_VERSION);
    const libDir = join(versionDir, "lib");
    mkdirSync(libDir, { recursive: true });
    // Microsoft's archive lays the library out under lib/, not the version root.
    writeFileSync(join(libDir, getOnnxLibraryName()), "stub");
    expect(findCachedOnnxRuntime(workDir)).toBe(libDir);
  });

  test("returns null when no library is present in either layout", () => {
    mkdirSync(join(workDir, "onnxruntime", ONNX_RUNTIME_VERSION), { recursive: true });
    expect(findCachedOnnxRuntime(workDir)).toBeNull();
  });
});

describe("bridge ONNX cached resolution (#71 stale metadata)", () => {
  test("anchors cleanup/meta to the version root while returning the lib directory", () => {
    const versionDir = join(workDir, "onnxruntime", bridgeOnnxTest.ORT_VERSION);
    const libDir = join(versionDir, "lib");
    const libName = "libonnxruntime.so";
    mkdirSync(libDir, { recursive: true });
    writeFileSync(join(libDir, libName), "manual archive layout");
    writeFileSync(
      join(versionDir, bridgeOnnxTest.ONNX_INSTALLED_META_FILE),
      JSON.stringify({
        version: bridgeOnnxTest.ORT_VERSION,
        installedAt: "2026-01-01T00:00:00.000Z",
        sha256: "stale-root-meta",
      }),
    );

    const resolvedDir = bridgeOnnxTest.resolveCachedOnnxRuntimeDir(versionDir, libName);

    expect(resolvedDir).toBe(libDir);
    expect(existsSync(join(resolvedDir, libName))).toBe(true);

    // Regression guard: with stale root metadata and a lib-only layout, the
    // post-lock cleanup target must be the version root. Cleaning the resolved
    // lib/ dir would treat it as unowned and delete the real library, causing a
    // re-download loop.
    bridgeOnnxTest.cleanupIncompleteTargetIfUnowned(versionDir);

    expect(existsSync(join(versionDir, bridgeOnnxTest.ONNX_INSTALLED_META_FILE))).toBe(true);
    expect(existsSync(join(libDir, libName))).toBe(true);
  });
});

describe("CLI ONNX manual install hint", () => {
  // Every platform the hint is asked about, including the ones the downloader
  // does not cover, so the two tables are compared over the same ground.
  const targets: Array<[NodeJS.Platform, string]> = [
    ["darwin", "arm64"],
    ["darwin", "x64"],
    ["linux", "x64"],
    ["linux", "arm64"],
    ["linux", "ia32"],
    ["win32", "x64"],
    ["win32", "arm64"],
    ["win32", "ia32"],
    ["freebsd", "x64"],
  ];

  const hintOn = (platform: NodeJS.Platform, arch: string): string =>
    withPlatform(platform, () => withArch(arch, () => getManualInstallHint()));

  test("promises an auto-download exactly where the downloader supports one", () => {
    // The downloader's own predicate is the authority: a hint that claims an
    // auto-download the downloader will not do, or sends the user to install
    // by hand on a platform it would have fetched, is wrong either way.
    const disagreeing = targets.filter(([platform, arch]) => {
      const promisesDownload = hintOn(platform, arch).includes("auto-downloads");
      const canDownload = withPlatform(platform, () =>
        withArch(arch, () => isOrtAutoDownloadSupported()),
      );
      return promisesDownload !== canDownload;
    });

    expect(disagreeing).toEqual([]);
  });

  test("names Homebrew only on the Mac Microsoft publishes no build for", () => {
    // macOS x64 is the single platform with no published ONNX Runtime archive.
    // Apple Silicon is an auto-download platform and was being told to brew it.
    const brewed = targets.filter(([platform, arch]) => hintOn(platform, arch).includes("brew"));

    expect(brewed).toEqual([["darwin", "x64"]]);
  });
});
