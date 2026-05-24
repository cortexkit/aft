/**
 * Tests for the LSP cache helpers consumed by `aft doctor`.
 *
 * The helpers must:
 *   - Report sizes for both subtrees (npm + GitHub) accurately
 *   - Sort entries by size desc (heaviest installs first)
 *   - URL-decode npm package directory names so users see real names
 *   - Not throw when a subtree is missing entirely
 *   - Clear both subtrees on demand and report bytes reclaimed
 *
 * Tests run against an isolated `AFT_CACHE_DIR` so they cannot pollute the
 * real user cache.
 */

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { acquireEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import { clearLspCaches, getLspCacheReport } from "../lib/lsp-cache.js";

let tmpRoot = "";
let releaseEnv: (() => void) | undefined;

beforeEach(async () => {
  tmpRoot = mkdtempSync(join(tmpdir(), "aft-cli-lsp-cache-"));
  releaseEnv = await acquireEnv({ AFT_CACHE_DIR: tmpRoot });
});

afterEach(() => {
  releaseEnv?.();
  releaseEnv = undefined;
  try {
    rmSync(tmpRoot, { recursive: true, force: true });
  } catch {
    // ignore
  }
});

function writeFakeNpmInstall(pkg: string, sizeBytes: number): void {
  const dir = join(tmpRoot, "lsp-packages", encodeURIComponent(pkg), "node_modules", ".bin");
  mkdirSync(dir, { recursive: true });
  // Write a single payload file of approximately the requested size.
  writeFileSync(join(dir, pkg.replace(/[@/]/g, "_")), Buffer.alloc(sizeBytes));
}

function writeFakeGithubInstall(id: string, sizeBytes: number): void {
  const dir = join(tmpRoot, "lsp-binaries", id, "bin");
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, id), Buffer.alloc(sizeBytes));
}

describe("getLspCacheReport", () => {
  test("returns empty report when neither subtree exists", () => {
    const report = getLspCacheReport();
    expect(report.npm.entries).toHaveLength(0);
    expect(report.github.entries).toHaveLength(0);
    expect(report.totalSize).toBe(0);
    expect(report.npm.path.endsWith("lsp-packages")).toBe(true);
    expect(report.github.path.endsWith("lsp-binaries")).toBe(true);
  });

  test("reports npm installs and decodes package names", () => {
    writeFakeNpmInstall("typescript-language-server", 1000);
    writeFakeNpmInstall("@vue/language-server", 2000);
    writeFakeNpmInstall("pyright", 500);

    const report = getLspCacheReport();
    expect(report.npm.entries).toHaveLength(3);
    // Sorted by size descending: vue (2000) > typescript (1000) > pyright (500)
    expect(report.npm.entries[0]?.name).toBe("@vue/language-server");
    expect(report.npm.entries[1]?.name).toBe("typescript-language-server");
    expect(report.npm.entries[2]?.name).toBe("pyright");

    // Sizes should be at least the payload size (filesystem block overhead may add).
    expect(report.npm.entries[0]?.size).toBeGreaterThanOrEqual(2000);
    expect(report.github.entries).toHaveLength(0);
    expect(report.totalSize).toBe(report.npm.totalSize);
  });

  test("reports github installs and combines totals", () => {
    writeFakeNpmInstall("typescript-language-server", 1000);
    writeFakeGithubInstall("clangd", 5000);
    writeFakeGithubInstall("zls", 3000);

    const report = getLspCacheReport();
    expect(report.npm.entries).toHaveLength(1);
    expect(report.github.entries).toHaveLength(2);
    expect(report.github.entries[0]?.name).toBe("clangd"); // bigger
    expect(report.github.entries[1]?.name).toBe("zls");
    expect(report.totalSize).toBe(report.npm.totalSize + report.github.totalSize);
  });
});

describe("clearLspCaches", () => {
  test("removes all entries from both subtrees and reports bytes reclaimed", () => {
    writeFakeNpmInstall("typescript-language-server", 1000);
    writeFakeNpmInstall("@vue/language-server", 2000);
    writeFakeGithubInstall("clangd", 5000);

    const before = getLspCacheReport();
    expect(before.npm.entries).toHaveLength(2);
    expect(before.github.entries).toHaveLength(1);

    const result = clearLspCaches();
    expect(result.cleared).toHaveLength(3);
    expect(result.errors).toHaveLength(0);
    expect(result.totalBytes).toBeGreaterThan(0);

    const after = getLspCacheReport();
    expect(after.npm.entries).toHaveLength(0);
    expect(after.github.entries).toHaveLength(0);
  });

  test("is a no-op when nothing is installed", () => {
    const result = clearLspCaches();
    expect(result.cleared).toHaveLength(0);
    expect(result.errors).toHaveLength(0);
    expect(result.totalBytes).toBe(0);
  });

  test("survives partial cleanup — root dirs aren't recreated empty", () => {
    writeFakeNpmInstall("pyright", 100);
    expect(existsSync(join(tmpRoot, "lsp-packages"))).toBe(true);
    clearLspCaches();
    // The package dir should be gone; the parent root remains (we don't
    // intentionally remove the root, just its contents).
    expect(existsSync(join(tmpRoot, "lsp-packages", encodeURIComponent("pyright")))).toBe(false);
  });
});
