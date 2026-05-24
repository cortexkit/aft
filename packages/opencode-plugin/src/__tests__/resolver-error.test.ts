/// <reference path="../bun-test.d.ts" />

/**
 * Resolver error-path tests.
 *
 * The resolver module now lives in @cortexkit/aft-bridge. Tests that required
 * module-level mocking of internal downloader paths are no longer feasible from
 * this package boundary — those belong in packages/aft-bridge tests. What we
 * can still test here is the public API surface:
 *
 *   1. `platformKey` throws with actionable message for unsupported platforms.
 *   2. `findBinarySync` returns null when no binary exists (validated by the
 *      existing e2e suite which runs against a real environment).
 *
 * The former subprocess-based mock tests are superseded by integration tests in
 * packages/aft-bridge/src/__tests__/ where the module boundary is correct.
 */

import { describe, expect, test } from "bun:test";
import { platformKey } from "@cortexkit/aft-bridge";

describe("resolver error paths", () => {
  test("includes supported platforms when a platform key is missing", () => {
    expect(() => platformKey("plan9", "x64")).toThrow(
      "Unsupported platform: plan9 (arch: x64). Supported platforms: darwin, linux, win32",
    );
  });

  test("platformKey returns correct key for supported darwin arm64", () => {
    expect(platformKey("darwin", "arm64")).toBe("darwin-arm64");
  });

  test("platformKey returns correct key for supported linux x64", () => {
    expect(platformKey("linux", "x64")).toBe("linux-x64");
  });
});
