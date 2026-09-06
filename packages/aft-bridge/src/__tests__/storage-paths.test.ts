import { describe, expect, test } from "bun:test";
import { join, resolve } from "node:path";

import {
  resolveAftStorageRoot,
  resolveCortexKitStorageRoot,
  type StoragePathContext,
  type StoragePlatform,
} from "../storage-paths.js";

describe("storage path ladder", () => {
  test("matches daemon data-home rungs on both platforms and ignores empty values", () => {
    const currentDirectory = resolve("audit-storage-cwd");
    const fallbackHome = join(currentDirectory, "system-home");
    const values: Record<string, string | undefined> = {
      AFT_STORAGE_DIR: "",
      AFT_CACHE_DIR: "",
      XDG_DATA_HOME: "",
      APPDATA: "",
      USERPROFILE: "",
      HOME: "",
      // LOCALAPPDATA must not affect this result: the shared Windows data-home
      // ladder uses roaming APPDATA or USERPROFILE.
      LOCALAPPDATA: join(currentDirectory, "wrong-local-data"),
    };
    const context = (platform: StoragePlatform): StoragePathContext => ({
      platform,
      fallbackHome,
      currentDirectory,
      lookup: (name) => values[name],
    });
    const relativeFallback = resolve(currentDirectory, ".local", "share", "cortexkit", "aft");

    for (const platform of ["other", "windows"] as const) {
      expect(resolveCortexKitStorageRoot(context(platform))).toBe(relativeFallback);
      expect(resolveAftStorageRoot("", context(platform))).toBe(relativeFallback);
    }

    values.HOME = join(currentDirectory, "home");
    values.USERPROFILE = join(currentDirectory, "profile");
    expect(resolveCortexKitStorageRoot(context("other"))).toBe(
      join(values.HOME, ".local", "share", "cortexkit", "aft"),
    );

    values.APPDATA = join(currentDirectory, "roaming");
    expect(resolveCortexKitStorageRoot(context("windows"))).toBe(
      join(values.APPDATA, "cortexkit", "aft"),
    );
    values.APPDATA = "";
    expect(resolveCortexKitStorageRoot(context("windows"))).toBe(
      join(values.USERPROFILE, "AppData", "Roaming", "cortexkit", "aft"),
    );

    values.XDG_DATA_HOME = "relative-data";
    expect(resolveCortexKitStorageRoot(context("other"))).toBe(
      resolve(currentDirectory, "relative-data", "cortexkit", "aft"),
    );

    values.AFT_CACHE_DIR = join(currentDirectory, "legacy-cache");
    expect(resolveCortexKitStorageRoot(context("other"))).toBe(join(values.AFT_CACHE_DIR, "aft"));
    expect(resolveAftStorageRoot("configured/../configured-aft", context("other"))).toBe(
      "configured/../configured-aft",
    );

    values.AFT_STORAGE_DIR = "~/operator-aft";
    expect(resolveAftStorageRoot("configured-aft", context("other"))).toBe(
      join(values.HOME, "operator-aft"),
    );
  });
});
