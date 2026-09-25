/// <reference path="../bun-test.d.ts" />

/**
 * Tests for `cleanupAbandonedOnnxAttempts`.
 *
 * Background: when the ONNX runtime download is interrupted (SIGKILL, host
 * crash, OS shutdown), it can leave behind:
 *   1. `.tmp.<pid>.<ts>/` staging dirs whose owning PID is dead
 *   2. A target install dir with no meta file (half-populated)
 *
 * The cleanup helper sweeps both before the next download attempt. Without
 * it, users would have to manually delete the AFT storage directory to
 * recover from a single SIGKILL during their first ONNX install.
 */

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import {
  existsSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readlinkSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { __onnxTest__ } from "../index.js";

const { cleanupAbandonedOnnxAttempts, ORT_VERSION, ONNX_INSTALLED_META_FILE } = __onnxTest__;
const {
  cleanupAbandonedStagingDirs,
  cleanupIncompleteTargetIfUnowned,
  copyOnnxLibraries,
  acquireLock,
  releaseLock,
} = __onnxTest__;

let workDir: string;
let onnxBaseDir: string;
let ortDir: string;

function withPlatform<T>(platform: NodeJS.Platform, fn: () => T): T {
  const descriptor = Object.getOwnPropertyDescriptor(process, "platform");
  Object.defineProperty(process, "platform", { configurable: true, value: platform });
  try {
    return fn();
  } finally {
    if (descriptor) Object.defineProperty(process, "platform", descriptor);
  }
}

beforeEach(() => {
  workDir = mkdtempSync(join(tmpdir(), "aft-onnx-cleanup-"));
  onnxBaseDir = join(workDir, "onnxruntime");
  ortDir = join(onnxBaseDir, ORT_VERSION);
  mkdirSync(onnxBaseDir, { recursive: true });
});

afterEach(() => {
  rmSync(workDir, { recursive: true, force: true });
});

describe("cleanupAbandonedOnnxAttempts", () => {
  test("removes staging dir owned by a dead PID", () => {
    // Create a `.tmp.<pid>.<ts>` directory using a PID that's almost certainly
    // dead (PID 999999 — well above any realistic concurrent process).
    const stagingDir = join(onnxBaseDir, `${ORT_VERSION}.tmp.999999.${Date.now()}`);
    mkdirSync(stagingDir, { recursive: true });
    writeFileSync(join(stagingDir, "scratch"), "placeholder");

    cleanupAbandonedOnnxAttempts(onnxBaseDir, ortDir);

    expect(existsSync(stagingDir)).toBe(false);
  });

  test("removes staging dir with malformed PID (NaN)", () => {
    // `.tmp.notapid.<ts>` — parseInt returns NaN, so the helper falls
    // through to "abandoned = true" since we can't verify liveness without
    // a valid pid.
    const stagingDir = join(onnxBaseDir, `${ORT_VERSION}.tmp.notapid.${Date.now()}`);
    mkdirSync(stagingDir, { recursive: true });

    cleanupAbandonedOnnxAttempts(onnxBaseDir, ortDir);

    expect(existsSync(stagingDir)).toBe(false);
  });

  test("preserves staging dir owned by the current process (still alive)", () => {
    // Use the test process's own PID — we know it's alive because we're
    // running in it. The cleanup must NOT remove an in-progress install.
    const stagingDir = join(onnxBaseDir, `${ORT_VERSION}.tmp.${process.pid}.${Date.now()}`);
    mkdirSync(stagingDir, { recursive: true });
    writeFileSync(join(stagingDir, "in-progress"), "still downloading");

    cleanupAbandonedOnnxAttempts(onnxBaseDir, ortDir);

    // On non-Windows, we use isProcessAlive — current PID is alive, so
    // the staging dir survives.
    if (process.platform !== "win32") {
      expect(existsSync(stagingDir)).toBe(true);
    }
    // On Windows we use mtime-based age comparison; a freshly-created dir
    // won't be old enough to trigger removal regardless of the PID.
  });

  test("ignores unrelated dirs that don't match the staging pattern", () => {
    // Files / dirs unrelated to the `.tmp.<pid>.<ts>` pattern must not be
    // touched, even if they live in the same parent dir.
    const unrelatedDir = join(onnxBaseDir, "some-other-thing");
    const unrelatedFile = join(onnxBaseDir, `${ORT_VERSION}.tar.gz`);
    mkdirSync(unrelatedDir, { recursive: true });
    writeFileSync(unrelatedFile, "downloaded archive");

    cleanupAbandonedOnnxAttempts(onnxBaseDir, ortDir);

    expect(existsSync(unrelatedDir)).toBe(true);
    expect(existsSync(unrelatedFile)).toBe(true);
  });

  test("removes half-populated install dir (target exists but no meta file)", () => {
    // Simulate a SIGKILL during the install: the version-named target dir
    // exists with some extracted content but the meta file was never written.
    // Cleanup must remove the dir so the next download can recreate cleanly.
    mkdirSync(ortDir, { recursive: true });
    writeFileSync(join(ortDir, "libonnxruntime.so.partial"), "incomplete");

    cleanupAbandonedOnnxAttempts(onnxBaseDir, ortDir);

    expect(existsSync(ortDir)).toBe(false);
  });

  test("pre-lock staging cleanup preserves target dir without meta", () => {
    mkdirSync(ortDir, { recursive: true });
    writeFileSync(join(ortDir, "libonnxruntime.so.partial"), "in-progress under another lock");

    cleanupAbandonedStagingDirs(onnxBaseDir);

    expect(existsSync(ortDir)).toBe(true);
    expect(existsSync(join(ortDir, "libonnxruntime.so.partial"))).toBe(true);
  });

  test("post-lock incomplete target cleanup removes target dir without meta", () => {
    mkdirSync(ortDir, { recursive: true });
    writeFileSync(join(ortDir, "libonnxruntime.so.partial"), "abandoned");

    cleanupIncompleteTargetIfUnowned(ortDir);

    expect(existsSync(ortDir)).toBe(false);
  });

  test("copies libraries into a target directory that does not exist yet", () => {
    // The installer stages into a fresh `<tmp>/install` directory that nothing
    // else creates; copying into it used to fail with ENOENT on copyfile.
    const extractedDir = join(workDir, "extracted-fresh-target");
    const targetDir = join(workDir, "staging", "install");
    mkdirSync(extractedDir, { recursive: true });
    writeFileSync(join(extractedDir, "libonnxruntime.so"), "binary");

    copyOnnxLibraries(
      { assetName: "onnxruntime-test", libName: "libonnxruntime.so", archiveType: "tgz" },
      extractedDir,
      targetDir,
      ["libonnxruntime.so"],
      [],
    );

    expect(existsSync(join(targetDir, "libonnxruntime.so"))).toBe(true);
  });

  test("required ONNX library copy failure removes partial target and throws", () => {
    const extractedDir = join(workDir, "extracted");
    const targetDir = join(workDir, "target");
    mkdirSync(extractedDir, { recursive: true });
    mkdirSync(targetDir, { recursive: true });
    writeFileSync(join(targetDir, "leftover"), "partial install");

    expect(() =>
      copyOnnxLibraries(
        { assetName: "onnxruntime-test", libName: "libonnxruntime.so", archiveType: "tgz" },
        extractedDir,
        targetDir,
        ["libonnxruntime.so"],
        [],
        () => {
          throw new Error("copy failed");
        },
      ),
    ).toThrow("copy failed");

    expect(existsSync(targetDir)).toBe(false);
  });

  test("missing required ONNX library after optional copies removes partial target", () => {
    const extractedDir = join(workDir, "extracted-missing");
    const targetDir = join(workDir, "target-missing");
    mkdirSync(extractedDir, { recursive: true });
    mkdirSync(targetDir, { recursive: true });

    expect(() =>
      copyOnnxLibraries(
        { assetName: "onnxruntime-test", libName: "libonnxruntime.so", archiveType: "tgz" },
        extractedDir,
        targetDir,
        [],
        [],
      ),
    ).toThrow("Required ONNX Runtime library missing");

    expect(existsSync(targetDir)).toBe(false);
  });

  test("rejects required symlinks that would escape the target directory after re-home", () => {
    const extractedDir = join(workDir, "extracted-escape");
    const targetDir = join(workDir, "target-escape");
    mkdirSync(extractedDir, { recursive: true });
    mkdirSync(targetDir, { recursive: true });
    writeFileSync(join(extractedDir, "libonnxruntime.so.1.24.4"), "binary");

    expect(() =>
      copyOnnxLibraries(
        { assetName: "onnxruntime-test", libName: "libonnxruntime.so", archiveType: "tgz" },
        extractedDir,
        targetDir,
        ["libonnxruntime.so.1.24.4"],
        [{ name: "libonnxruntime.so", target: "../.aft-onnx-installing" }],
      ),
    ).toThrow("points outside install dir");

    expect(existsSync(targetDir)).toBe(false);
  });

  test.skipIf(process.platform === "win32")(
    "allows same-directory versioned ONNX symlinks during re-home",
    () => {
      const extractedDir = join(workDir, "extracted-legit-symlink");
      const targetDir = join(workDir, "target-legit-symlink");
      mkdirSync(extractedDir, { recursive: true });
      mkdirSync(targetDir, { recursive: true });
      writeFileSync(join(extractedDir, "libonnxruntime.so.1.24.4"), "binary");

      copyOnnxLibraries(
        { assetName: "onnxruntime-test", libName: "libonnxruntime.so", archiveType: "tgz" },
        extractedDir,
        targetDir,
        ["libonnxruntime.so.1.24.4"],
        [{ name: "libonnxruntime.so", target: "libonnxruntime.so.1.24.4" }],
      );

      const installedLink = join(targetDir, "libonnxruntime.so");
      expect(lstatSync(installedLink).isSymbolicLink()).toBe(true);
      expect(readlinkSync(installedLink)).toBe("libonnxruntime.so.1.24.4");
      expect(existsSync(installedLink)).toBe(true);
    },
  );

  test("Windows install lock reclaims a fresh lock whose owner PID is already dead", () => {
    const lockPath = join(onnxBaseDir, ".aft-onnx-installing");
    writeFileSync(lockPath, "999999999\n2026-01-01T00:00:00.000Z\n");

    const acquired = withPlatform("win32", () => acquireLock(lockPath));

    expect(acquired).toBe(true);
    expect(readFileSync(lockPath, "utf8").split(/\r?\n/, 1)[0]).toBe(String(process.pid));
    releaseLock(lockPath);
    expect(existsSync(lockPath)).toBe(false);
  });

  test("preserves complete install dir (meta file present)", () => {
    // The meta file is the "install completed" signal. If it's there, the
    // dir is healthy and must NOT be removed even though its name matches
    // the version pattern.
    mkdirSync(ortDir, { recursive: true });
    writeFileSync(join(ortDir, "libonnxruntime.so"), "real lib");
    writeFileSync(join(ortDir, ONNX_INSTALLED_META_FILE), '{"version":"x"}');

    cleanupAbandonedOnnxAttempts(onnxBaseDir, ortDir);

    expect(existsSync(ortDir)).toBe(true);
    expect(existsSync(join(ortDir, ONNX_INSTALLED_META_FILE))).toBe(true);
  });

  test("handles missing onnxBaseDir gracefully (no throw)", () => {
    // First call is allowed when the user has never attempted ONNX install
    // before — base dir doesn't exist. Helper must swallow the readdir
    // error rather than crash plugin startup.
    const nonexistent = join(workDir, "never-created");
    const targetInside = join(nonexistent, ORT_VERSION);

    expect(() => cleanupAbandonedOnnxAttempts(nonexistent, targetInside)).not.toThrow();
  });
});
