import { expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { resolveCortexKitProjectConfigPath } from "@cortexkit/aft-bridge";
import { formatDoctorStorageStatus } from "../commands/doctor.js";
import { runConfigFix } from "../lib/feature-plan.js";
import { evaluatePluginLoad } from "../lib/plugin-load.js";

test("doctor cache root uses the user base and ignores harness and project redirects", () => {
  const scratch = mkdtempSync(join(tmpdir(), "aft-cache-doctor-"));
  try {
    const user = join(scratch, "user.jsonc");
    const project = join(scratch, "project");
    const projectConfig = resolveCortexKitProjectConfigPath(project);
    mkdirSync(dirname(projectConfig), { recursive: true });
    writeFileSync(
      user,
      JSON.stringify({
        storage: { cache_dir: "/base" },
        harnesses: { pi: { storage: { cache_dir: "~/cache" } } },
      }),
    );
    writeFileSync(
      projectConfig,
      JSON.stringify({
        storage: { cache_dir: "/evil" },
        harnesses: { pi: { storage: { cache_dir: "/evil-pi" } } },
      }),
    );
    const result = evaluatePluginLoad({
      userConfigPath: user,
      projectDirectory: project,
      harness: "pi",
      home: scratch,
    });
    expect(result.cacheDir).toBe("/base");
  } finally {
    rmSync(scratch, { recursive: true, force: true });
  }
});

test("doctor storage status names the separate configured cache root", () => {
  const harness = {
    pluginRegistered: true,
    storageDir: { path: "/durable", exists: true, sizesByKey: {}, cacheRoot: "/cache" },
  } as Parameters<typeof formatDoctorStorageStatus>[0];
  expect(formatDoctorStorageStatus(harness)).toContain("cache root: /cache");
  expect(formatDoctorStorageStatus(harness)).toContain("/durable");
});

test("doctor names the typed refusal for a first-use same-device cache parent", () => {
  const scratch = mkdtempSync(join(tmpdir(), "aft-cache-device-doctor-"));
  try {
    const durable = join(scratch, "storage");
    const parent = join(scratch, "cache");
    mkdirSync(durable);
    mkdirSync(parent);
    const harness = {
      pluginRegistered: true,
      storageDir: { path: durable, exists: true, sizesByKey: {}, cacheRoot: join(parent, "aft") },
    } as Parameters<typeof formatDoctorStorageStatus>[0];
    expect(formatDoctorStorageStatus(harness)).toContain("cache_dir_on_storage_device");
  } finally {
    rmSync(scratch, { recursive: true, force: true });
  }
});

test("doctor keeps native inactive-cache cleanup outcomes", () => {
  const result = runConfigFix(() => ({
    ok: true,
    stdout: JSON.stringify({ files: [], cache_cleanup: { removed: ["/old/index"], error: null } }),
    stderr: "",
    status: 0,
  }));
  expect(result).toEqual({
    ok: true,
    files: [],
    cacheCleanup: { removed: ["/old/index"], error: null },
  });
});

test("doctor preserves removals and partial progress alongside a cleanup failure", () => {
  const cleanup = {
    removed: ["/old/index"],
    skipped: ["/unknown: unmarked"],
    partial: [{ path: "/old/views", removed_entries: 3 }],
    error: "another root is in use",
  };
  const result = runConfigFix(() => ({
    ok: false,
    stdout: JSON.stringify({ files: [], cache_cleanup: cleanup }),
    stderr: "",
    status: 1,
  }));
  expect(result).toEqual({ ok: true, files: [], cacheCleanup: cleanup });
});

test("doctor preserves removed-with-warning cleanup outcomes", () => {
  const cleanup = {
    removed: ["/old/index"],
    warnings: ["removed_with_warning: legacy disposition accepted; pending until handles close"],
    error: null,
  };
  const result = runConfigFix(() => ({
    ok: true,
    stdout: JSON.stringify({ files: [], cache_cleanup: cleanup }),
    stderr: "",
    status: 0,
  }));
  expect(result).toEqual({ ok: true, files: [], cacheCleanup: cleanup });
});
