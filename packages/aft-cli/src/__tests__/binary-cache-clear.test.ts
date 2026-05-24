/// <reference path="../bun-test.d.ts" />

/**
 * Tests for `clearOldBinaries`.
 *
 * The user-facing contract: `aft doctor --clear` (with the binary-cache
 * option selected) deletes every cached `aft` binary version EXCEPT the
 * one matching the running CLI. Keeping the active version protects a
 * live OpenCode/Pi process that's currently executing from that binary.
 *
 * Tests use AFT_CACHE_DIR to redirect `getAftBinaryCacheDir()` away from
 * `~/.cache/aft/`, so we don't disturb the user's real cache.
 */

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { acquireEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import { clearOldBinaries } from "../commands/doctor.js";
import { getSelfVersion } from "../lib/self-version.js";

let workDir: string;
let releaseEnv: (() => void) | undefined;

beforeEach(async () => {
  workDir = mkdtempSync(join(tmpdir(), "aft-binary-cache-clear-"));
  releaseEnv = await acquireEnv({ AFT_CACHE_DIR: workDir });
});

afterEach(() => {
  releaseEnv?.();
  releaseEnv = undefined;
  rmSync(workDir, { recursive: true, force: true });
});

function seedBinary(version: string) {
  const dir = join(workDir, "bin", version);
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, "aft"), "fake binary");
}

describe("clearOldBinaries", () => {
  test("removes old versions but keeps the version matching the running CLI", () => {
    const cli = getSelfVersion();
    const keep = `v${cli.replace(/^v/, "")}`;
    const oldA = "v0.1.0";
    const oldB = "v0.10.0";

    // Pick old versions that aren't the current CLI version.
    if (keep === oldA || keep === oldB) {
      // Defensive: bump if we ever accidentally collide with a real
      // historical tag the test ships with.
      throw new Error(`test fixture collides with running CLI version ${keep}`);
    }

    seedBinary(keep);
    seedBinary(oldA);
    seedBinary(oldB);

    const result = clearOldBinaries();

    expect(result.cleared).toBe(2);
    expect(result.errors).toEqual([]);
    expect(result.keptVersion).toBe(keep);

    // Critical: the active version must still be on disk after the clear.
    expect(existsSync(join(workDir, "bin", keep))).toBe(true);
    expect(existsSync(join(workDir, "bin", oldA))).toBe(false);
    expect(existsSync(join(workDir, "bin", oldB))).toBe(false);
  });

  test("returns cleared=0 when only the active version is present", () => {
    const cli = getSelfVersion();
    seedBinary(`v${cli.replace(/^v/, "")}`);

    const result = clearOldBinaries();
    expect(result.cleared).toBe(0);
    expect(result.bytesReclaimed).toBe(0);
    expect(result.errors).toEqual([]);
  });

  test("returns cleared=0 when the cache directory does not exist", () => {
    // No bin/ dir was ever created.
    const result = clearOldBinaries();
    expect(result.cleared).toBe(0);
    expect(result.bytesReclaimed).toBe(0);
    expect(result.errors).toEqual([]);
  });

  test("safety: never removes the version matching the running CLI", () => {
    const cli = getSelfVersion();
    const keep = `v${cli.replace(/^v/, "")}`;
    seedBinary(keep);
    seedBinary("v0.1.0");
    seedBinary("v0.2.0");
    seedBinary("v0.3.0");

    clearOldBinaries();

    // The running CLI's binary must always survive — anything else is
    // a regression that could yank the binary out from under a live
    // OpenCode/Pi bridge process.
    expect(existsSync(join(workDir, "bin", keep))).toBe(true);
  });
});
