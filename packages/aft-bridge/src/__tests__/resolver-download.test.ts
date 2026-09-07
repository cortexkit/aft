/// <reference path="../bun-test.d.ts" />

/**
 * findBinary expectedVersion test — uses real fs (no module mocking).
 *
 * History: an earlier version of this test used `mock.module("node:fs", …)`
 * with no-op stubs to force every sync resolution path to miss. Bun runs all
 * test files in the same process, so the partial node:fs mock leaked into
 * concurrent test files (notably `onnx-cleanup.test.ts`) and caused ENOENT in
 * any test that called `writeFileSync` after this file's mocks were installed
 * but before they were restored. `mock.restore()` does NOT undo
 * `mock.module(…)` in Bun, so the partial mock could not be cleaned up.
 *
 * Today the test uses a real empty temp directory as the AFT cache, real
 * `node_modules`-free environment, and injects only the downloader boundary.
 * All other sync resolution paths miss naturally because the temp cache is
 * empty and the npm platform package isn't installed.
 */
import { afterEach, beforeEach, describe, expect, mock, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { __setEnsureBinaryForTests, findBinary } from "../resolver.js";
import { acquireEnv } from "./test-utils/env-guard.js";

describe("findBinary async download", () => {
  let cacheDir: string;
  let releaseEnv: (() => void) | undefined;

  beforeEach(async () => {
    cacheDir = mkdtempSync(join(tmpdir(), "aft-resolver-test-"));
    // Empty the cache + PATH + HOME so every sync resolution path misses
    // naturally, forcing the async download fallback to run.
    releaseEnv = await acquireEnv({
      // CI exports the freshly built binary here for the e2e suites; it is the
      // first resolution step and would satisfy findBinary before any fallback.
      AFT_BINARY_PATH: undefined,
      AFT_CACHE_DIR: cacheDir,
      PATH: "",
      HOME: cacheDir,
    });
  });

  afterEach(() => {
    __setEnsureBinaryForTests(null);
    releaseEnv?.();
    releaseEnv = undefined;
    rmSync(cacheDir, { recursive: true, force: true });
    mock.restore();
  });

  test("honors expectedVersion when falling through to ensureBinary", async () => {
    const seenVersions: Array<string | undefined> = [];
    __setEnsureBinaryForTests(async (version?: string) => {
      seenVersions.push(version);
      return "/downloaded/aft";
    });

    await expect(findBinary("0.99.0-test")).resolves.toBe("/downloaded/aft");
    expect(seenVersions).toEqual(["0.99.0-test"]);
  });

  test("logs auto-download as the successful resolution source", () => {
    const packageRoot = join(import.meta.dir, "..", "..");
    const result = spawnSync(
      process.execPath,
      [
        "-e",
        [
          'import { __setEnsureBinaryForTests, findBinary } from "./src/resolver.ts";',
          '__setEnsureBinaryForTests(async () => "/downloaded/aft");',
          'console.log(await findBinary("0.99.0-test"));',
        ].join(" "),
      ],
      {
        cwd: packageRoot,
        env: {
          ...process.env,
          AFT_CACHE_DIR: cacheDir,
          HOME: cacheDir,
          PATH: "",
        },
        encoding: "utf8",
      },
    );

    expect(result.error).toBeUndefined();
    expect(result.status).toBe(0);
    expect(result.stdout.trim()).toBe("/downloaded/aft");
    expect(result.stderr).toContain(
      "[aft-bridge] Resolved binary from auto-download: /downloaded/aft",
    );
  });
});
