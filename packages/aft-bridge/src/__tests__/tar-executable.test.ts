/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { execTarExtractionSync, windowsTarExecutable } from "../tar-executable.js";

let workDir: string;

beforeEach(() => {
  workDir = mkdtempSync(join(tmpdir(), "aft-windows-tar-"));
});

afterEach(() => {
  rmSync(workDir, { recursive: true, force: true });
});

describe("windowsTarExecutable", () => {
  test("bypasses a GNU tar.exe shadowing System32 on PATH", () => {
    const pathDir = join(workDir, "msys2", "usr", "bin");
    const systemRoot = join(workDir, "Windows");
    const systemTar = join(systemRoot, "System32", "tar.exe");
    mkdirSync(pathDir, { recursive: true });
    mkdirSync(join(systemRoot, "System32"), { recursive: true });

    const shadowTar = join(pathDir, "tar.exe");
    writeFileSync(shadowTar, "tar: Cannot connect to C: resolve failed\n");
    writeFileSync(systemTar, "Windows bsdtar fixture\n");

    const executable = windowsTarExecutable({
      platform: "win32",
      env: { PATH: `${pathDir};${process.env.PATH ?? ""}`, SystemRoot: systemRoot },
    });

    const spawned: string[] = [];
    const recordSpawn = (command: string, args: string[]): void => {
      spawned.push(command);
      if (command === "tar" || command === "tar.exe" || command === shadowTar) {
        if (args.some((arg) => arg.includes("C:"))) {
          throw new Error("tar: Cannot connect to C: resolve failed");
        }
      }
    };

    expect(() => recordSpawn(executable, ["-xf", "C:\\Users\\me\\runtime.zip"])).not.toThrow();
    expect(spawned).toEqual([systemTar]);
  });

  test("falls back to bare tar when System32 tar.exe is unavailable", () => {
    expect(
      windowsTarExecutable({
        platform: "win32",
        env: { SystemRoot: join(workDir, "missing") },
      }),
    ).toBe("tar");
  });

  test("uses bare tar on non-Windows platforms", () => {
    expect(windowsTarExecutable({ platform: "linux" })).toBe("tar");
  });

  test("names the resolved executable when extraction fails", () => {
    const executable = windowsTarExecutable();
    const missingArchive = join(workDir, "C:", "missing.zip");

    expect(() => execTarExtractionSync(["-xf", missingArchive, "-C", workDir], 2_000)).toThrow(
      `tar extraction failed using ${executable}`,
    );
  });
});
