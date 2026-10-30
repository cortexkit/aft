/// <reference path="../bun-test.d.ts" />

/**
 * Tests for `clearOldBinaries`.
 *
 * The user-facing contract: `aft doctor --clear` (with the binary-cache
 * option selected) retains the running CLI's binary and versions installed or
 * pinned by harness plugins. Keeping those versions avoids breaking live or
 * offline OpenCode/Pi sessions.
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

describe("binary prune safety", () => {
  test("unknown CLI version leaves every cached binary untouched", () => {
    seedBinary("v0.1.0");
    seedBinary("v0.2.0");
    const result = clearOldBinaries("unknown", []);
    expect(result.cleared).toBe(0);
    expect(result.keptVersion).toBeNull();
    expect(existsSync(join(workDir, "bin", "v0.1.0"))).toBe(true);
    expect(existsSync(join(workDir, "bin", "v0.2.0"))).toBe(true);
  });

  test("unreadable plugin registration leaves every cached binary untouched", () => {
    seedBinary("v0.1.0");
    const result = clearOldBinaries(getSelfVersion(), [
      {
        pluginPackageName: "@cortexkit/aft-pi",
        getPluginCacheInfo: () => {
          throw new Error("unreadable registration");
        },
      },
    ]);
    expect(result.cleared).toBe(0);
    expect(existsSync(join(workDir, "bin", "v0.1.0"))).toBe(true);
  });
});

describe("plugin binary retention", () => {
  test("installed and exact configured plugin versions survive binary pruning", () => {
    for (const version of ["v0.1.0", "v0.2.0", "v0.3.0"]) seedBinary(version);
    const result = clearOldBinaries(getSelfVersion(), [
      {
        pluginPackageName: "@cortexkit/aft-pi",
        getPluginCacheInfo: () => ({
          path: workDir,
          exists: true,
          cached: "0.1.0",
          configuredEntry: "npm:@cortexkit/aft-pi@0.2.0",
        }),
      },
    ]);
    expect(result.cleared).toBe(1);
    expect(existsSync(join(workDir, "bin", "v0.1.0"))).toBe(true);
    expect(existsSync(join(workDir, "bin", "v0.2.0"))).toBe(true);
    expect(existsSync(join(workDir, "bin", "v0.3.0"))).toBe(false);
  });

  test("a configured exact pin survives before the plugin is installed", () => {
    seedBinary("v0.1.0");
    seedBinary("v0.2.0");
    const result = clearOldBinaries(getSelfVersion(), [
      {
        pluginPackageName: "@cortexkit/aft-pi",
        getPluginCacheInfo: () => ({ path: workDir, exists: false }),
        getConfiguredPluginEntry: () => "npm:@cortexkit/aft-pi@0.1.0",
      },
    ]);
    expect(result.cleared).toBe(1);
    expect(existsSync(join(workDir, "bin", "v0.1.0"))).toBe(true);
    expect(existsSync(join(workDir, "bin", "v0.2.0"))).toBe(false);
  });
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
