/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { withEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";

import { buildConfigTierConfigureParams } from "../config.js";
import { resolveCortexKitStorageRoot } from "../shared/storage-paths.js";

describe("OpenCode storage root resolution", () => {
  test("honors the legacy cache rung plus absent, empty, and explicit storage overrides", async () => {
    const root = mkdtempSync(join(tmpdir(), "aft-opencode-storage-paths-"));
    try {
      const dataHome = join(root, "xdg-data");
      const project = join(root, "project");
      mkdirSync(project, { recursive: true });
      await withEnv(
        {
          HOME: root,
          XDG_DATA_HOME: dataHome,
          XDG_CONFIG_HOME: join(root, "xdg-config"),
          AFT_CACHE_DIR: join(root, "legacy-cache"),
          AFT_STORAGE_DIR: undefined,
        },
        () => {
          expect(resolveCortexKitStorageRoot()).toBe(join(root, "legacy-cache", "aft"));
          process.env.AFT_CACHE_DIR = "";
          expect(resolveCortexKitStorageRoot()).toBe(join(dataHome, "cortexkit", "aft"));
          process.env.AFT_STORAGE_DIR = "";
          expect(resolveCortexKitStorageRoot()).toBe(join(dataHome, "cortexkit", "aft"));
          process.env.AFT_STORAGE_DIR = "./local/../local-aft";
          const resolved = resolveCortexKitStorageRoot();
          expect(resolved).toBe(resolve("./local-aft"));

          const params = buildConfigTierConfigureParams(project, { storage_dir: resolved });
          expect(params.storage_dir).toBe(resolved);
        },
      );
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });
});
