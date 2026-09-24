/// <reference path="../bun-test.d.ts" />

/**
 * A system ONNX Runtime with a compatible version number can still be
 * unloadable. A Homebrew runtime linked against an abseil release that was
 * since upgraded away fails dlopen with "Library not loaded:
 * /opt/homebrew/opt/abseil/lib/libabsl_...dylib", and the resolver used to
 * pick it over AFT's own downloaded runtime anyway. These tests build minimal
 * Mach-O images whose load commands name real or missing dependencies and pin
 * both the probe and the resolver's fall-back to the managed download.
 */

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { __onnxTest__, probeOnnxRuntimeLoadable } from "../index.js";

const { detectOnnxVersion, resolveOnnxRuntime } = __onnxTest__;

const LC_LOAD_DYLIB = 0xc;
const LC_LOAD_WEAK_DYLIB = 0x80000018;
const CPU_TYPE_ARM64 = 0x0100000c;
const CPU_TYPE_X86_64 = 0x01000007;
const HOST_CPU = process.arch === "x64" ? CPU_TYPE_X86_64 : CPU_TYPE_ARM64;
const LIB = "libonnxruntime.dylib";

/** A 64-bit little-endian Mach-O dylib header followed by one command per dependency. */
function machO(dependencies: Array<{ path: string; weak?: boolean }>, cpuType = HOST_CPU): Buffer {
  const commands = dependencies.map(({ path, weak }) => {
    const name = Buffer.from(`${path}\0`, "utf8");
    const size = Math.ceil((24 + name.length) / 8) * 8;
    const command = Buffer.alloc(size);
    command.writeUInt32LE(weak ? LC_LOAD_WEAK_DYLIB : LC_LOAD_DYLIB, 0);
    command.writeUInt32LE(size, 4);
    command.writeUInt32LE(24, 8);
    name.copy(command, 24);
    return command;
  });
  const body = Buffer.concat(commands);
  const header = Buffer.alloc(32);
  header.writeUInt32LE(0xfeedfacf, 0);
  header.writeUInt32LE(cpuType, 4);
  header.writeUInt32LE(6, 12);
  header.writeUInt32LE(commands.length, 16);
  header.writeUInt32LE(body.length, 20);
  return Buffer.concat([header, body]);
}

let workDir: string;
let presentDependency: string;
let missingDependency: string;

beforeEach(() => {
  workDir = mkdtempSync(join(tmpdir(), "aft-onnx-loadability-"));
  presentDependency = join(workDir, "abseil", "libabsl_present.dylib");
  missingDependency = join(workDir, "abseil", "libabsl_random_distributions.2601.0.0.dylib");
  mkdirSync(join(workDir, "abseil"), { recursive: true });
  writeFileSync(presentDependency, "dependency");
});

afterEach(() => {
  rmSync(workDir, { recursive: true, force: true });
});

describe("probeOnnxRuntimeLoadable", () => {
  test("a missing absolute dependency makes the library unloadable and names it", () => {
    const lib = join(workDir, LIB);
    writeFileSync(lib, machO([{ path: presentDependency }, { path: missingDependency }]));

    expect(probeOnnxRuntimeLoadable(lib)).toEqual({
      loadable: false,
      reason: `missing dependency ${missingDependency}`,
    });
  });

  test("present, rpath-relative, system-cache, and weak dependencies all load", () => {
    const lib = join(workDir, LIB);
    writeFileSync(
      lib,
      machO([
        { path: presentDependency },
        { path: "@rpath/libonnxruntime_providers_shared.dylib" },
        { path: "/usr/lib/libc++.1.dylib" },
        { path: "/System/Library/Frameworks/Foundation.framework/Foundation" },
        { path: missingDependency, weak: true },
      ]),
    );

    expect(probeOnnxRuntimeLoadable(lib)).toEqual({ loadable: true, checked: true });
  });

  test("a library built for another architecture is unloadable", () => {
    const lib = join(workDir, LIB);
    const other = HOST_CPU === CPU_TYPE_ARM64 ? CPU_TYPE_X86_64 : CPU_TYPE_ARM64;
    writeFileSync(lib, machO([{ path: presentDependency }], other));

    const probe = probeOnnxRuntimeLoadable(lib);
    expect(probe.loadable).toBe(false);
  });

  test("the probe follows the bare-name link to the real image", () => {
    const real = join(workDir, "libonnxruntime.1.28.0.dylib");
    writeFileSync(real, machO([{ path: missingDependency }]));
    symlinkSync(real, join(workDir, LIB));

    expect(probeOnnxRuntimeLoadable(join(workDir, LIB)).loadable).toBe(false);
  });

  test("formats it cannot read are reported unchecked, not rejected", () => {
    const lib = join(workDir, "libonnxruntime.so");
    writeFileSync(lib, Buffer.from([0x7f, 0x45, 0x4c, 0x46, 2, 1, 1, 0]));

    expect(probeOnnxRuntimeLoadable(lib)).toEqual({ loadable: true, checked: false });
  });
});

describe("detectOnnxVersion", () => {
  test("reports the version of the file the bare name loads, not a stale sibling", () => {
    const keg = join(workDir, "Cellar", "onnxruntime", "1.28.0", "lib");
    mkdirSync(keg, { recursive: true });
    writeFileSync(join(keg, "libonnxruntime.1.28.0.dylib"), "binary");
    const lib = join(workDir, "lib");
    mkdirSync(lib);
    writeFileSync(join(lib, "libonnxruntime.1.30.0.dylib"), "stale");
    symlinkSync(join(keg, "libonnxruntime.1.28.0.dylib"), join(lib, LIB));

    expect(detectOnnxVersion(lib, LIB)).toBe("1.28.0");
  });
});

describe.skipIf(process.platform === "win32")("ONNX Runtime resolution order", () => {
  const platformInfo = { assetName: "onnxruntime-test", libName: LIB, archiveType: "tgz" as const };

  function systemRuntime(dependency: string): string {
    const dir = join(workDir, "homebrew", "lib");
    mkdirSync(dir, { recursive: true });
    writeFileSync(join(dir, "libonnxruntime.1.28.0.dylib"), machO([{ path: dependency }]));
    symlinkSync("libonnxruntime.1.28.0.dylib", join(dir, LIB));
    return dir;
  }

  function fakeDownload(calls: string[]) {
    return async (_info: unknown, targetDir: string) => {
      calls.push(targetDir);
      mkdirSync(targetDir, { recursive: true });
      writeFileSync(join(targetDir, LIB), "managed runtime");
      return targetDir;
    };
  }

  test("a system runtime present but unloadable falls back to the downloaded one", async () => {
    const systemDir = systemRuntime(missingDependency);
    const downloads: string[] = [];

    const resolved = await resolveOnnxRuntime(join(workDir, "storage"), {
      platformInfo,
      systemSearchPaths: [systemDir],
      download: fakeDownload(downloads),
    });

    expect(downloads).toHaveLength(1);
    expect(resolved).toBe(downloads[0]);
    expect(resolved).not.toBe(systemDir);
  });

  test("a loadable compatible system runtime is still used without a download", async () => {
    const systemDir = systemRuntime(presentDependency);
    const downloads: string[] = [];

    const resolved = await resolveOnnxRuntime(join(workDir, "storage"), {
      platformInfo,
      systemSearchPaths: [systemDir],
      download: fakeDownload(downloads),
    });

    expect(resolved).toBe(systemDir);
    expect(downloads).toEqual([]);
  });
});
