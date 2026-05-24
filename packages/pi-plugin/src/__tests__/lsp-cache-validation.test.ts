import { afterEach, describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { acquireEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import { runAutoInstall } from "../lsp-auto-install.js";
import { runGithubAutoInstall } from "../lsp-github-install.js";

const roots = new Set<string>();
let releaseEnv: (() => void) | undefined;

async function tempCache(): Promise<string> {
  const root = mkdtempSync(join(tmpdir(), "aft-lsp-cache-validation-"));
  roots.add(root);
  releaseEnv = await acquireEnv({ AFT_CACHE_DIR: root });
  return root;
}

afterEach(() => {
  releaseEnv?.();
  releaseEnv = undefined;
  for (const root of roots) rmSync(root, { recursive: true, force: true });
  roots.clear();
});

describe("cached LSP validation before lsp_paths_extra", () => {
  test("npm cached binary with mismatched sha is excluded and quarantined", async () => {
    const root = await tempCache();
    const pkgDir = join(root, "lsp-packages", "pyright");
    const binDir = join(pkgDir, "node_modules", ".bin");
    mkdirSync(binDir, { recursive: true });
    // pyright's binary in NPM_LSP_TABLE is `pyright-langserver` (the LSP),
    // not the `pyright` CLI. isInstalled only fires when the actual spec.binary
    // exists, so write that exact name.
    writeFileSync(join(binDir, "pyright-langserver"), "tampered");
    writeFileSync(
      join(pkgDir, ".aft-installed"),
      JSON.stringify({ version: "1.1.300", installedAt: "now", sha256: "0".repeat(64) }),
    );

    const result = runAutoInstall(root, {
      autoInstall: false,
      graceDays: 7,
      versions: {},
      disabled: new Set(),
    });

    expect(result.cachedBinDirs).not.toContain(binDir);
    const quarantine = join(root, "lsp-packages", ".quarantine", "pyright");
    expect(existsSync(quarantine)).toBe(true);
    expect(readdirSync(quarantine).length).toBeGreaterThan(0);
  });

  test("GitHub cached binary with mismatched sha is excluded and quarantined", async () => {
    const root = await tempCache();
    const pkgDir = join(root, "lsp-binaries", "clangd");
    const binDir = join(pkgDir, "bin");
    mkdirSync(binDir, { recursive: true });
    writeFileSync(join(binDir, "clangd"), "tampered");
    // Use binarySha256 (v0.27 metadata shape). Legacy sha256-only entries
    // trigger a TOFU migration that accepts the current binary hash, so a
    // tampered binary with a stale sha256 alone would no longer quarantine.
    // binarySha256 is the authoritative field and must match exactly.
    writeFileSync(
      join(pkgDir, ".aft-installed"),
      JSON.stringify({
        version: "21.1.0",
        installedAt: "now",
        binarySha256: "0".repeat(64),
      }),
    );

    const result = runGithubAutoInstall(new Set(), {
      autoInstall: false,
      graceDays: 7,
      versions: {},
      disabled: new Set(),
    });

    expect(result.cachedBinDirs).not.toContain(binDir);
    const quarantine = join(root, "lsp-binaries", ".quarantine", "clangd");
    expect(existsSync(quarantine)).toBe(true);
    expect(readdirSync(quarantine).length).toBeGreaterThan(0);
  });
});
