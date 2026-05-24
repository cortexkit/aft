/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, mock, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { __setSpawnSyncForTests, getMigrationStatus } from "../migration.js";
import { acquireEnv } from "./test-utils/env-guard.js";

describe("storage migration status", () => {
  let tempDir: string;
  let releaseEnv: (() => void) | undefined;

  beforeEach(async () => {
    tempDir = mkdtempSync(join(tmpdir(), "aft-migration-status-test-"));
    releaseEnv = await acquireEnv({
      XDG_DATA_HOME: tempDir,
      HOME: tempDir,
    });
  });

  afterEach(() => {
    __setSpawnSyncForTests(null);
    releaseEnv?.();
    releaseEnv = undefined;
    rmSync(tempDir, { recursive: true, force: true });
    mock.restore();
  });

  function setSpawnResult(result: unknown) {
    const spawnSync = mock(() => result);
    __setSpawnSyncForTests(spawnSync as never);
    return spawnSync;
  }

  test("getMigrationStatus_returns_migrated_true_when_marker_exists", async () => {
    const payload = {
      harness: "opencode",
      target_root: join(tempDir, "cortexkit", "aft"),
      migrated: true,
      marker_path: join(tempDir, "cortexkit", "aft", "opencode", ".migrated_from_legacy"),
      migrated_at: "2026-05-19T15:00:00.123Z",
      source_path: "/legacy/aft",
      aft_version: "0.27.0",
    };
    const spawnSync = setSpawnResult({
      status: 0,
      signal: null,
      error: undefined,
      stdout: `${JSON.stringify(payload)}\n`,
      stderr: "",
    });

    await expect(
      getMigrationStatus({ harness: "opencode", binaryPath: "/bin/aft" }),
    ).resolves.toEqual(payload);
    expect(spawnSync).toHaveBeenCalledWith(
      "/bin/aft",
      [
        "migrate-storage",
        "--status",
        "--to",
        join(tempDir, "cortexkit", "aft"),
        "--harness",
        "opencode",
      ],
      expect.any(Object),
    );
  });

  test("getMigrationStatus_returns_migrated_false_when_marker_absent", async () => {
    const payload = {
      harness: "opencode",
      target_root: join(tempDir, "cortexkit", "aft"),
      migrated: false,
    };
    setSpawnResult({
      status: 0,
      signal: null,
      error: undefined,
      stdout: `${JSON.stringify(payload)}\n`,
      stderr: "",
    });

    await expect(
      getMigrationStatus({ harness: "opencode", binaryPath: "/bin/aft" }),
    ).resolves.toEqual(payload);
  });

  test("getMigrationStatus_throws_on_invalid_json", async () => {
    setSpawnResult({
      status: 0,
      signal: null,
      error: undefined,
      stdout: "not-json\n",
      stderr: "",
    });

    await expect(
      getMigrationStatus({ harness: "opencode", binaryPath: "/bin/aft" }),
    ).rejects.toThrow(/invalid JSON/);
  });

  test("getMigrationStatus_handles_nonzero_exit", async () => {
    setSpawnResult({
      status: 1,
      signal: null,
      error: undefined,
      stdout: "",
      stderr: "failed",
    });

    await expect(
      getMigrationStatus({ harness: "opencode", binaryPath: "/bin/aft" }),
    ).rejects.toThrow(/exit 1.*failed/);
  });
});
