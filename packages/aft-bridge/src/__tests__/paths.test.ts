/// <reference path="../bun-test.d.ts" />

import { afterEach, describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  markAnnouncementSeen,
  repairRootScopedStorageFile,
  resolveCortexKitConfigPaths,
  resolveCortexKitProjectConfigPath,
  resolveCortexKitUserConfigPath,
  resolveHarnessStoragePath,
  shouldShowAnnouncement,
} from "../paths.js";

const tempRoots = new Set<string>();

function createStorageRoot(): string {
  const root = mkdtempSync(join(tmpdir(), "aft-bridge-paths-"));
  tempRoots.add(root);
  return root;
}

afterEach(() => {
  for (const root of tempRoots) {
    rmSync(root, { recursive: true, force: true });
  }
  tempRoots.clear();
});

describe("CortexKit config paths", () => {
  const originalHome = process.env.HOME;
  const originalUserProfile = process.env.USERPROFILE;
  const originalXdg = process.env.XDG_CONFIG_HOME;

  // `process.env.KEY = undefined` stores the string "undefined"; a variable that
  // was unset before the test must be deleted so later-spawned processes do not
  // inherit USERPROFILE=undefined (or XDG_CONFIG_HOME=undefined on macOS).
  const restoreEnv = (key: string, value: string | undefined) => {
    if (value === undefined) {
      delete process.env[key];
    } else {
      process.env[key] = value;
    }
  };

  afterEach(() => {
    restoreEnv("HOME", originalHome);
    restoreEnv("USERPROFILE", originalUserProfile);
    restoreEnv("XDG_CONFIG_HOME", originalXdg);
  });

  test("user config honors absolute XDG_CONFIG_HOME", () => {
    const root = createStorageRoot();
    process.env.HOME = join(root, "home");
    process.env.XDG_CONFIG_HOME = join(root, "xdg");

    expect(resolveCortexKitUserConfigPath()).toBe(join(root, "xdg", "cortexkit", "aft.jsonc"));
  });

  test("user config falls back to home .config when XDG_CONFIG_HOME is relative", () => {
    const root = createStorageRoot();
    process.env.HOME = join(root, "home");
    process.env.XDG_CONFIG_HOME = "relative-xdg";

    expect(resolveCortexKitUserConfigPath()).toBe(
      join(root, "home", ".config", "cortexkit", "aft.jsonc"),
    );
  });

  test("project config lives under .cortexkit", () => {
    const root = createStorageRoot();

    expect(resolveCortexKitProjectConfigPath(root)).toBe(join(root, ".cortexkit", "aft.jsonc"));
    expect(resolveCortexKitConfigPaths(root).projectConfigPath).toBe(
      join(root, ".cortexkit", "aft.jsonc"),
    );
  });
});

describe("harness storage paths", () => {
  test("resolveHarnessStoragePath scopes paths by harness", () => {
    const root = createStorageRoot();

    expect(resolveHarnessStoragePath(root, "opencode", "last_announced_version")).toBe(
      join(root, "opencode", "last_announced_version"),
    );
  });

  test("repairRootScopedStorageFile moves root copy when harness copy is absent", () => {
    const root = createStorageRoot();
    writeFileSync(join(root, "last-update-check.json"), "{}", "utf8");

    const path = repairRootScopedStorageFile(root, "opencode", "last-update-check.json");

    expect(path).toBe(join(root, "opencode", "last-update-check.json"));
    expect(existsSync(join(root, "last-update-check.json"))).toBe(false);
    expect(readFileSync(path, "utf8")).toBe("{}");
  });

  test("repairRootScopedStorageFile does not overwrite existing harness copy", () => {
    const root = createStorageRoot();
    writeFileSync(join(root, "last_announced_version"), "root", "utf8");
    const harnessPath = resolveHarnessStoragePath(root, "pi", "last_announced_version");
    mkdirSync(join(root, "pi"), { recursive: true });
    writeFileSync(harnessPath, "harness", "utf8");

    const path = repairRootScopedStorageFile(root, "pi", "last_announced_version");

    expect(path).toBe(harnessPath);
    expect(readFileSync(path, "utf8")).toBe("harness");
    expect(readFileSync(join(root, "last_announced_version"), "utf8")).toBe("root");
  });
});

describe("shouldShowAnnouncement", () => {
  // Per magic-context#99: a fresh install or ephemeral sandbox (Docker, CI,
  // disposable dev container) has no last_announced_version file yet. The
  // pre-fix behavior was to treat that as "no previous match" → show the
  // changelog dialog. That spammed every restart in a wiped-storage sandbox
  // and confused first-time users with bullet points they had no context to
  // interpret.
  //
  // Post-fix behavior: silently seed the marker on the first launch we see
  // and suppress the dialog. Real upgrades from a persisted older version
  // still surface.

  test("returns false and seeds marker on first install (no marker file)", () => {
    const root = createStorageRoot();
    const markerPath = resolveHarnessStoragePath(root, "opencode", "last_announced_version");
    expect(existsSync(markerPath)).toBe(false);

    expect(shouldShowAnnouncement(root, "opencode", "0.30.3")).toBe(false);

    // Seeded — next launch sees the file already matches and stays quiet
    // without having to re-call this function.
    expect(existsSync(markerPath)).toBe(true);
    expect(readFileSync(markerPath, "utf8")).toBe("0.30.3");
    expect(shouldShowAnnouncement(root, "opencode", "0.30.3")).toBe(false);
  });

  test("returns false when marker matches current version", () => {
    const root = createStorageRoot();
    const markerPath = resolveHarnessStoragePath(root, "opencode", "last_announced_version");
    mkdirSync(join(root, "opencode"), { recursive: true });
    writeFileSync(markerPath, "0.30.3", "utf8");

    expect(shouldShowAnnouncement(root, "opencode", "0.30.3")).toBe(false);
  });

  test("returns true when marker holds a different (older) version", () => {
    const root = createStorageRoot();
    const markerPath = resolveHarnessStoragePath(root, "opencode", "last_announced_version");
    mkdirSync(join(root, "opencode"), { recursive: true });
    writeFileSync(markerPath, "0.29.1", "utf8");

    expect(shouldShowAnnouncement(root, "opencode", "0.30.3")).toBe(true);

    // Should NOT have written until the caller calls markAnnouncementSeen.
    expect(readFileSync(markerPath, "utf8")).toBe("0.29.1");
  });

  test("treats whitespace-only marker like a fresh install (seeds it)", () => {
    const root = createStorageRoot();
    const markerPath = resolveHarnessStoragePath(root, "opencode", "last_announced_version");
    mkdirSync(join(root, "opencode"), { recursive: true });
    writeFileSync(markerPath, "   \n", "utf8");

    expect(shouldShowAnnouncement(root, "opencode", "0.30.3")).toBe(false);
    expect(readFileSync(markerPath, "utf8")).toBe("0.30.3");
  });

  test("returns false when currentVersion is empty (announcement disabled)", () => {
    const root = createStorageRoot();
    expect(shouldShowAnnouncement(root, "opencode", "")).toBe(false);

    // Must NOT seed when there's no version to seed with — otherwise a later
    // call with a real version would mis-classify as an upgrade.
    expect(existsSync(resolveHarnessStoragePath(root, "opencode", "last_announced_version"))).toBe(
      false,
    );
  });

  test("scopes seen state per harness so OpenCode upgrades don't suppress Pi's first launch", () => {
    const root = createStorageRoot();

    // OpenCode has been around: persisted older version.
    const ocMarker = resolveHarnessStoragePath(root, "opencode", "last_announced_version");
    mkdirSync(join(root, "opencode"), { recursive: true });
    writeFileSync(ocMarker, "0.29.1", "utf8");

    // Pi is a fresh install on the same machine.
    expect(shouldShowAnnouncement(root, "opencode", "0.30.3")).toBe(true);
    expect(shouldShowAnnouncement(root, "pi", "0.30.3")).toBe(false);

    // Pi's first-install path silently seeded its own per-harness marker.
    expect(
      readFileSync(resolveHarnessStoragePath(root, "pi", "last_announced_version"), "utf8"),
    ).toBe("0.30.3");
    // And it did NOT touch OpenCode's marker.
    expect(readFileSync(ocMarker, "utf8")).toBe("0.29.1");
  });
});

describe("markAnnouncementSeen", () => {
  test("writes the current version to the per-harness marker", () => {
    const root = createStorageRoot();
    const markerPath = resolveHarnessStoragePath(root, "opencode", "last_announced_version");

    markAnnouncementSeen(root, "opencode", "0.30.3");

    expect(readFileSync(markerPath, "utf8")).toBe("0.30.3");
    // Subsequent shouldShowAnnouncement reads now return false.
    expect(shouldShowAnnouncement(root, "opencode", "0.30.3")).toBe(false);
  });

  test("is a no-op when currentVersion is empty", () => {
    const root = createStorageRoot();
    const markerPath = resolveHarnessStoragePath(root, "opencode", "last_announced_version");

    markAnnouncementSeen(root, "opencode", "");

    expect(existsSync(markerPath)).toBe(false);
  });
});
