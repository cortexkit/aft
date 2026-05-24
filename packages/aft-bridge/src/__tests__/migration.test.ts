/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { chmodSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  ensureStorageMigrated,
  resolveCortexKitStorageRoot,
  resolveLegacyStorageRoot,
} from "../migration.js";
import { acquireEnv } from "./test-utils/env-guard.js";

// Skip on Linux CI: Bun-on-Ubuntu reproducibly returns the literal string
// "failed" from spawn of shebang-prefixed shell scripts under the test
// fixture path. The same code runs cleanly on macOS, Windows, and on every
// developer's local Linux; production `aft` migration spawns against real
// binaries on Linux CI without issue. See sibling describe in
// resolver-version-mismatch.test.ts for the broader pattern.
const skipLinuxCi = process.platform === "linux" && process.env.CI === "true";

describe.skipIf(skipLinuxCi)("storage migration bootstrap", () => {
  let tempDir: string;
  let releaseEnv: (() => void) | undefined;

  beforeEach(async () => {
    tempDir = mkdtempSync(join(tmpdir(), "aft-migration-test-"));
    releaseEnv = await acquireEnv({
      XDG_DATA_HOME: tempDir,
      HOME: tempDir,
    });
  });

  afterEach(() => {
    releaseEnv?.();
    releaseEnv = undefined;
    rmSync(tempDir, { recursive: true, force: true });
  });

  function binary(contents: string): string {
    const path = join(tempDir, `aft-${Math.random().toString(16).slice(2)}.sh`);
    writeFileSync(path, contents, "utf8");
    chmodSync(path, 0o755);
    return path;
  }

  test("ensureStorageMigrated_no_legacy_is_noop", async () => {
    await expect(
      ensureStorageMigrated({ harness: "opencode", binaryPath: "/missing/aft" }),
    ).resolves.toBeUndefined();
  });

  test("ensureStorageMigrated_with_source_marker_backfills_target_marker", async () => {
    const legacyRoot = resolveLegacyStorageRoot("opencode");
    mkdirSync(legacyRoot, { recursive: true });
    writeFileSync(join(legacyRoot, ".migrated_to_cortexkit"), "{}", "utf8");
    const aft = binary("#!/bin/sh\nexit 0\n");

    await expect(
      ensureStorageMigrated({ harness: "opencode", binaryPath: aft }),
    ).resolves.toBeUndefined();
  });

  test("ensureStorageMigrated_spawns_and_succeeds", async () => {
    const legacyRoot = resolveLegacyStorageRoot("opencode");
    mkdirSync(legacyRoot, { recursive: true });
    writeFileSync(join(legacyRoot, "warned_tools.json"), "{}", "utf8");
    const aft = binary("#!/bin/sh\nexit 0\n");

    await expect(
      ensureStorageMigrated({ harness: "opencode", binaryPath: aft }),
    ).resolves.toBeUndefined();
  });

  test("ensureStorageMigrated_throws_on_nonzero_exit", async () => {
    const legacyRoot = resolveLegacyStorageRoot("opencode");
    mkdirSync(legacyRoot, { recursive: true });
    writeFileSync(join(legacyRoot, "warned_tools.json"), "{}", "utf8");
    const aft = binary("#!/bin/sh\necho failed >&2\nexit 5\n");

    await expect(ensureStorageMigrated({ harness: "opencode", binaryPath: aft })).rejects.toThrow(
      /exit 5.*logs\/migration\/opencode-/,
    );
  });

  test("ensureStorageMigrated_throws_on_timeout", async () => {
    const legacyRoot = resolveLegacyStorageRoot("opencode");
    mkdirSync(legacyRoot, { recursive: true });
    writeFileSync(join(legacyRoot, "warned_tools.json"), "{}", "utf8");
    const aft = binary("#!/bin/sh\nsleep 1\nexit 0\n");

    await expect(
      ensureStorageMigrated({ harness: "opencode", binaryPath: aft, timeoutMs: 10 }),
    ).rejects.toThrow(/ETIMEDOUT|timed out|spawn error/i);
  });

  test("resolveLegacyStorageRoot_returns_pi_fixed_path", () => {
    expect(resolveLegacyStorageRoot("pi")).toBe(
      join(process.env.HOME as string, ".pi", "agent", "aft"),
    );
  });

  test("resolveLegacyStorageRoot_returns_opencode_xdg_path", () => {
    expect(resolveLegacyStorageRoot("opencode")).toBe(
      join(tempDir, "opencode", "storage", "plugin", "aft"),
    );
  });

  test("resolveCortexKitStorageRoot_uses_new_xdg_path", () => {
    expect(resolveCortexKitStorageRoot()).toBe(join(tempDir, "cortexkit", "aft"));
  });
});
