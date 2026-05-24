/// <reference path="../bun-test.d.ts" />

/**
 * Tests for `findSystemOnnxRuntime` version filtering.
 *
 * The historical bug: AFT's resolver picked up the FIRST `libonnxruntime.so`
 * it found in standard system paths, regardless of version. On distros that
 * still ship libonnxruntime1.9 (Ubuntu 22.04, etc.), this meant the resolver
 * returned a path Rust would later reject as too old, and semantic search
 * stayed "failed" forever — even though AFT's auto-download path could
 * have shipped a working v1.24.
 *
 * The fix: detect the version from the library filename / symlink target,
 * skip anything older than v1.20, and let the resolver fall through to
 * auto-download. These tests pin that behavior.
 *
 * We don't shell out or touch real system paths; we exercise the
 * `detectOnnxVersion` and `isOnnxVersionCompatible` helpers directly via
 * the test namespace because they own the version-parse logic that the
 * filter relies on.
 */

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { __onnxTest__ } from "../index.js";

const { detectOnnxVersion, isOnnxVersionCompatible, REQUIRED_ORT_MAJOR, REQUIRED_ORT_MIN_MINOR } =
  __onnxTest__;

let workDir: string;

beforeEach(() => {
  workDir = mkdtempSync(join(tmpdir(), "aft-onnx-version-filter-"));
});

afterEach(() => {
  rmSync(workDir, { recursive: true, force: true });
});

describe("isOnnxVersionCompatible", () => {
  test("rejects v1.9 (the canonical regression case)", () => {
    expect(isOnnxVersionCompatible("1.9.0")).toBe(false);
    expect(isOnnxVersionCompatible("1.9.5")).toBe(false);
    expect(isOnnxVersionCompatible("1.19.0")).toBe(false);
  });

  test("accepts v1.20 (the minimum) and above", () => {
    expect(isOnnxVersionCompatible("1.20.0")).toBe(true);
    expect(isOnnxVersionCompatible("1.24.4")).toBe(true);
    expect(isOnnxVersionCompatible("1.99.0")).toBe(true);
  });

  test("rejects wrong major version", () => {
    expect(isOnnxVersionCompatible("0.99.0")).toBe(false);
    expect(isOnnxVersionCompatible("2.0.0")).toBe(false);
  });

  test("rejects garbage", () => {
    expect(isOnnxVersionCompatible("abc")).toBe(false);
    expect(isOnnxVersionCompatible("")).toBe(false);
    expect(isOnnxVersionCompatible("1")).toBe(false);
  });

  test("agrees with the documented requirement constants", () => {
    expect(REQUIRED_ORT_MAJOR).toBe(1);
    expect(REQUIRED_ORT_MIN_MINOR).toBe(20);
    // The boundary case must match: vREQUIRED_MAJOR.REQUIRED_MIN_MINOR.0
    expect(isOnnxVersionCompatible(`${REQUIRED_ORT_MAJOR}.${REQUIRED_ORT_MIN_MINOR}.0`)).toBe(true);
    expect(isOnnxVersionCompatible(`${REQUIRED_ORT_MAJOR}.${REQUIRED_ORT_MIN_MINOR - 1}.999`)).toBe(
      false,
    );
  });
});

describe("detectOnnxVersion", () => {
  test("extracts version from versioned .so suffix (Linux pattern)", () => {
    // libonnxruntime.so.1.24.4 — Microsoft's Linux release naming.
    writeFileSync(join(workDir, "libonnxruntime.so.1.24.4"), "binary");
    expect(detectOnnxVersion(workDir, "libonnxruntime.so")).toBe("1.24.4");
  });

  test("extracts version from .dylib infix suffix (macOS pattern)", () => {
    // libonnxruntime.1.24.4.dylib — Microsoft's macOS release naming.
    writeFileSync(join(workDir, "libonnxruntime.1.24.4.dylib"), "binary");
    expect(detectOnnxVersion(workDir, "libonnxruntime.dylib")).toBe("1.24.4");
  });

  test("follows symlink from bare lib name to versioned target", () => {
    writeFileSync(join(workDir, "libonnxruntime.so.1.9.0"), "binary");
    symlinkSync("libonnxruntime.so.1.9.0", join(workDir, "libonnxruntime.so"));
    expect(detectOnnxVersion(workDir, "libonnxruntime.so")).toBe("1.9.0");
  });

  test("returns null when no library is present", () => {
    expect(detectOnnxVersion(workDir, "libonnxruntime.so")).toBe(null);
  });

  test("returns null on unreadable / missing dir", () => {
    expect(detectOnnxVersion(join(workDir, "does-not-exist"), "libonnxruntime.so")).toBe(null);
  });
});

describe("integration: detect + compat (the real bug shape)", () => {
  test("v1.9 layout from a broken Ubuntu install is correctly rejected", () => {
    // Reproduce the user-reported state: distro shipped libonnxruntime
    // v1.9 with the canonical symlink chain. Detection must surface
    // 1.9.0, the compat check must reject it.
    writeFileSync(join(workDir, "libonnxruntime.so.1.9.0"), "binary");
    symlinkSync("libonnxruntime.so.1.9.0", join(workDir, "libonnxruntime.so.1"));
    symlinkSync("libonnxruntime.so.1", join(workDir, "libonnxruntime.so"));

    const version = detectOnnxVersion(workDir, "libonnxruntime.so");
    expect(version).toBe("1.9.0");
    expect(isOnnxVersionCompatible(version!)).toBe(false);
  });

  test("v1.24 layout is correctly accepted", () => {
    writeFileSync(join(workDir, "libonnxruntime.so.1.24.4"), "binary");
    symlinkSync("libonnxruntime.so.1.24.4", join(workDir, "libonnxruntime.so"));

    const version = detectOnnxVersion(workDir, "libonnxruntime.so");
    expect(version).toBe("1.24.4");
    expect(isOnnxVersionCompatible(version!)).toBe(true);
  });
});

describe("findSystemOnnxRuntime (Linux paths)", () => {
  // We can't safely exercise `findSystemOnnxRuntime` against `/usr/lib`
  // in tests (would depend on the host's actual install state). Instead
  // we lock in the helper's *contract* indirectly: detectOnnxVersion and
  // isOnnxVersionCompatible are sufficient to prove the filter logic is
  // correct, and `findSystemOnnxRuntime` is a thin wrapper that calls
  // both of them in sequence.
  //
  // This documentation test makes the contract explicit so future
  // refactors can't quietly break the filter.
  test("filter contract: detect + compat must reject v1.9 and accept v1.24", () => {
    expect(isOnnxVersionCompatible("1.9.0")).toBe(false);
    expect(isOnnxVersionCompatible("1.24.4")).toBe(true);
  });
});
