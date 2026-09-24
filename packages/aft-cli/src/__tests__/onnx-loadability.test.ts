/// <reference path="../bun-test.d.ts" />

/**
 * doctor used to report a Homebrew ONNX Runtime as "systemVersion 1.30.0,
 * systemCompatible true" while the library could not be loaded (its abseil
 * dependency had been upgraded away) and the version came from a stale
 * sibling file. These tests pin that doctor's system inspection skips an
 * unloadable runtime and reports it as ignored with the reason.
 */

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { detectOrtVersion, getOnnxLibraryName, inspectSystemOnnxRuntime } from "../lib/onnx.js";

const CPU_TYPE = process.arch === "x64" ? 0x01000007 : 0x0100000c;

/** Minimal 64-bit Mach-O dylib whose only load command requires `dependency`. */
function machO(dependency: string): Buffer {
  const name = Buffer.from(`${dependency}\0`, "utf8");
  const size = Math.ceil((24 + name.length) / 8) * 8;
  const command = Buffer.alloc(size);
  command.writeUInt32LE(0xc, 0);
  command.writeUInt32LE(size, 4);
  command.writeUInt32LE(24, 8);
  name.copy(command, 24);
  const header = Buffer.alloc(32);
  header.writeUInt32LE(0xfeedfacf, 0);
  header.writeUInt32LE(CPU_TYPE, 4);
  header.writeUInt32LE(6, 12);
  header.writeUInt32LE(1, 16);
  header.writeUInt32LE(size, 20);
  return Buffer.concat([header, command]);
}

let workDir: string;

beforeEach(() => {
  workDir = mkdtempSync(join(tmpdir(), "aft-cli-onnx-loadability-"));
});

afterEach(() => {
  rmSync(workDir, { recursive: true, force: true });
});

function systemDir(dependency: string): string {
  const libName = getOnnxLibraryName();
  const dir = join(workDir, "lib");
  mkdirSync(dir, { recursive: true });
  const versioned =
    libName === "libonnxruntime.so" ? `${libName}.1.28.0` : "libonnxruntime.1.28.0.dylib";
  writeFileSync(join(dir, versioned), machO(dependency));
  symlinkSync(versioned, join(dir, libName));
  return dir;
}

describe.skipIf(process.platform === "win32")("doctor system ONNX Runtime inspection", () => {
  test("an unloadable system runtime is ignored with the missing dependency as reason", () => {
    const missing = join(workDir, "abseil", "libabsl_random_distributions.2601.0.0.dylib");
    const dir = systemDir(missing);

    expect(inspectSystemOnnxRuntime([dir])).toEqual({
      path: null,
      ignored: { path: dir, reason: `unloadable: missing dependency ${missing} — ignored` },
    });
  });

  test("a loadable system runtime is reported as the system path", () => {
    const present = join(workDir, "libdep.dylib");
    writeFileSync(present, "dependency");
    const dir = systemDir(present);

    expect(inspectSystemOnnxRuntime([dir])).toEqual({ path: dir, ignored: null });
  });

  test("the reported version is the file the bare name loads, not a stale sibling", () => {
    const libName = getOnnxLibraryName();
    const versioned = (version: string) =>
      libName === "libonnxruntime.so" ? `${libName}.${version}` : `libonnxruntime.${version}.dylib`;
    // Homebrew layout: the bare name links into a versioned keg, and an old
    // versioned file was left behind next to the link.
    const keg = join(workDir, "Cellar", "onnxruntime", "1.28.0", "lib");
    mkdirSync(keg, { recursive: true });
    writeFileSync(join(keg, versioned("1.28.0")), "runtime");
    const dir = join(workDir, "lib");
    mkdirSync(dir, { recursive: true });
    writeFileSync(join(dir, versioned("1.30.0")), "stale");
    symlinkSync(join(keg, versioned("1.28.0")), join(dir, libName));

    expect(detectOrtVersion(dir)).toBe("1.28.0");
  });
});
