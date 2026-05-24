/**
 * Tests for the per-install version metadata helpers in lsp-cache.ts.
 *
 * Audit v0.17 #4: persisting the installed version lets us detect a
 * `lsp.versions` pin change and trigger a transparent reinstall.
 */

import { afterEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { withEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import {
  readInstalledMeta,
  readInstalledMetaIn,
  writeInstalledMeta,
  writeInstalledMetaIn,
} from "../lsp-cache.js";

const tempRoots = new Set<string>();

function tempCacheRoot(): string {
  const root = mkdtempSync(join(tmpdir(), "aft-installed-meta-"));
  tempRoots.add(root);
  return root;
}

afterEach(() => {
  for (const root of tempRoots) rmSync(root, { recursive: true, force: true });
  tempRoots.clear();
});

describe("writeInstalledMeta / readInstalledMeta (npm path)", () => {
  test("round-trip: writes then reads the same version string", async () => {
    await withEnv({ AFT_CACHE_DIR: tempCacheRoot() }, () => {
      writeInstalledMeta("typescript-language-server", "5.3.2");
      const meta = readInstalledMeta("typescript-language-server");
      expect(meta).not.toBeNull();
      expect(meta?.version).toBe("5.3.2");
      expect(meta?.installedAt).toMatch(/^\d{4}-\d{2}-\d{2}T/);
    });
  });

  test("returns null when no metadata file exists", async () => {
    await withEnv({ AFT_CACHE_DIR: tempCacheRoot() }, () => {
      expect(readInstalledMeta("typescript-language-server")).toBeNull();
    });
  });

  test("returns null on corrupt JSON", async () => {
    const root = tempCacheRoot();
    await withEnv({ AFT_CACHE_DIR: root }, () => {
      const dir = join(root, "lsp-packages", "pyright");
      mkdirSync(dir, { recursive: true });
      writeFileSync(join(dir, ".aft-installed"), "not json {");
      expect(readInstalledMeta("pyright")).toBeNull();
    });
  });

  test("returns null when version field is missing", async () => {
    const root = tempCacheRoot();
    await withEnv({ AFT_CACHE_DIR: root }, () => {
      const dir = join(root, "lsp-packages", "pyright");
      mkdirSync(dir, { recursive: true });
      writeFileSync(join(dir, ".aft-installed"), JSON.stringify({ installedAt: "2024-01-01" }));
      expect(readInstalledMeta("pyright")).toBeNull();
    });
  });

  test("scoped npm package works (uses URL-encoded directory)", async () => {
    await withEnv({ AFT_CACHE_DIR: tempCacheRoot() }, () => {
      writeInstalledMeta("@vue/language-server", "2.0.0");
      const meta = readInstalledMeta("@vue/language-server");
      expect(meta?.version).toBe("2.0.0");
    });
  });
});

describe("writeInstalledMetaIn / readInstalledMetaIn (GitHub path / arbitrary dir)", () => {
  test("round-trip into an arbitrary directory", () => {
    const root = tempCacheRoot();
    const installDir = join(root, "lsp-binaries", "clangd");
    writeInstalledMetaIn(installDir, "21.1.0");
    const meta = readInstalledMetaIn(installDir);
    expect(meta?.version).toBe("21.1.0");
  });

  test("creates the directory if it doesn't exist yet", () => {
    const root = tempCacheRoot();
    const installDir = join(root, "deeply", "nested", "missing");
    writeInstalledMetaIn(installDir, "1.0.0");
    expect(readInstalledMetaIn(installDir)?.version).toBe("1.0.0");
  });

  test("subsequent writes overwrite the previous version", () => {
    const root = tempCacheRoot();
    const installDir = join(root, "pkg");
    writeInstalledMetaIn(installDir, "1.0.0");
    writeInstalledMetaIn(installDir, "2.0.0");
    expect(readInstalledMetaIn(installDir)?.version).toBe("2.0.0");
  });
});

// Audit v0.17 #1: TOFU verification persists the SHA-256 of the downloaded
// archive so a second install of the same tag can detect a tampered release.
describe("InstalledMeta sha256 (TOFU verification)", () => {
  test("sha256 round-trips when provided", () => {
    const root = tempCacheRoot();
    const installDir = join(root, "clangd");
    const hash = "a".repeat(64);
    writeInstalledMetaIn(installDir, "21.1.0", hash);
    const meta = readInstalledMetaIn(installDir);
    expect(meta?.version).toBe("21.1.0");
    expect(meta?.sha256).toBe(hash);
  });

  test("sha256 is omitted when not provided (npm install path)", () => {
    const root = tempCacheRoot();
    const installDir = join(root, "pyright");
    writeInstalledMetaIn(installDir, "1.1.300");
    const meta = readInstalledMetaIn(installDir);
    expect(meta?.version).toBe("1.1.300");
    expect(meta?.sha256).toBeUndefined();
  });

  test("empty-string sha256 is read back as undefined (legacy / corrupt input)", () => {
    const root = tempCacheRoot();
    const installDir = join(root, "pkg");
    mkdirSync(installDir, { recursive: true });
    writeFileSync(
      join(installDir, ".aft-installed"),
      JSON.stringify({ version: "1.0.0", installedAt: "now", sha256: "" }),
    );
    expect(readInstalledMetaIn(installDir)?.sha256).toBeUndefined();
  });

  test("backwards compatible: pre-v0.17 metadata files read fine without sha256", () => {
    const root = tempCacheRoot();
    const installDir = join(root, "pkg");
    mkdirSync(installDir, { recursive: true });
    writeFileSync(
      join(installDir, ".aft-installed"),
      JSON.stringify({ version: "1.0.0", installedAt: "2024-01-01" }),
    );
    const meta = readInstalledMetaIn(installDir);
    expect(meta?.version).toBe("1.0.0");
    expect(meta?.sha256).toBeUndefined();
  });
});
