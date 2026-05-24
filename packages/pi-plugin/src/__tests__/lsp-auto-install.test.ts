import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { acquireEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import { type AutoInstallConfig, runAutoInstall } from "../lsp-auto-install";
import { lspBinaryPath, writeInstalledMeta } from "../lsp-cache";

const DAY_MS = 24 * 60 * 60 * 1000;

function isoDaysAgo(now: number, days: number): string {
  return new Date(now - days * DAY_MS).toISOString();
}

let tempCache: string;
let tempProject: string;
let releaseEnv: (() => void) | undefined;

beforeEach(async () => {
  tempCache = mkdtempSync(join(tmpdir(), "aft-lsp-autoinstall-cache-"));
  tempProject = mkdtempSync(join(tmpdir(), "aft-lsp-autoinstall-project-"));
  releaseEnv = await acquireEnv({ AFT_CACHE_DIR: tempCache });
});

afterEach(() => {
  releaseEnv?.();
  releaseEnv = undefined;
  rmSync(tempCache, { recursive: true, force: true });
  rmSync(tempProject, { recursive: true, force: true });
});

/**
 * Make a fetch mock that returns an npm-registry-shaped response with
 * a single old version, ensuring the grace filter passes on first probe.
 */
function fakeFetch(): typeof fetch {
  return (async () => {
    const now = Date.now();
    return {
      ok: true,
      status: 200,
      async json() {
        return {
          time: {
            "1.0.0": isoDaysAgo(now, 60),
          },
          "dist-tags": { latest: "1.0.0" },
        };
      },
    } as Response;
  }) as typeof fetch;
}

function defaultConfig(overrides: Partial<AutoInstallConfig> = {}): AutoInstallConfig {
  return {
    autoInstall: true,
    graceDays: 7,
    versions: {},
    disabled: new Set(),
    ...overrides,
  };
}

/**
 * Pre-populate the cache as if a binary were already installed for the
 * given npm package. Writes the binary AND a valid installed-metadata
 * record so Lane F's TOFU sha256 validation accepts the cache entry.
 */
function fakeInstalled(npmPackage: string, binary: string): string {
  const path = lspBinaryPath(npmPackage, binary);
  mkdirSync(join(path, ".."), { recursive: true });
  const content = "#!/bin/sh\nexit 0\n";
  writeFileSync(path, content);
  const sha256 = createHash("sha256").update(content).digest("hex");
  // Version is arbitrary but must pass isSafeVersion (semver-ish).
  writeInstalledMeta(npmPackage, "1.0.0", sha256);
  return path;
}

describe("runAutoInstall", () => {
  test("returns no cached paths when nothing is installed", async () => {
    // Empty project — no relevant servers, nothing to install.
    const result = await runAutoInstall(tempProject, defaultConfig(), fakeFetch());
    expect(result.cachedBinDirs).toHaveLength(0);
    expect(result.installsStarted).toBe(0);
    // Most servers skipped as "not relevant to project".
    expect(result.skipped.length).toBeGreaterThan(0);
  });

  test("surfaces already-installed binaries even when project is empty", async () => {
    fakeInstalled("typescript-language-server", "typescript-language-server");
    const result = await runAutoInstall(tempProject, defaultConfig(), fakeFetch());
    expect(result.cachedBinDirs).toHaveLength(1);
    expect(result.cachedBinDirs[0]).toContain("typescript-language-server");
  });

  test("disabled config blocks discovery for that server", async () => {
    // Make project relevant to TypeScript by creating a package.json.
    writeFileSync(join(tempProject, "package.json"), "{}");
    const result = await runAutoInstall(
      tempProject,
      defaultConfig({ disabled: new Set(["typescript", "biome"]) }),
      fakeFetch(),
    );
    // Disabled entries appear in skipped with reason "disabled by config".
    const disabledEntries = result.skipped.filter((s) => s.reason.includes("disabled"));
    expect(disabledEntries.map((s) => s.id)).toContain("typescript");
    expect(disabledEntries.map((s) => s.id)).toContain("biome");
  });

  test("autoInstall=false skips installs but still surfaces cached paths", async () => {
    fakeInstalled("yaml-language-server", "yaml-language-server");
    writeFileSync(join(tempProject, "package.json"), "{}");
    const result = await runAutoInstall(
      tempProject,
      defaultConfig({ autoInstall: false }),
      fakeFetch(),
    );
    expect(result.cachedBinDirs).toHaveLength(1);
    expect(result.installsStarted).toBe(0);
  });

  test("project-relevance: package.json triggers TypeScript discovery", async () => {
    writeFileSync(join(tempProject, "package.json"), "{}");
    const result = await runAutoInstall(tempProject, defaultConfig(), fakeFetch());
    // TS is relevant; not in skipped.
    const tsSkipped = result.skipped.find((s) => s.id === "typescript");
    expect(tsSkipped).toBeUndefined();
    // Install should have started.
    expect(result.installsStarted).toBeGreaterThan(0);
  });

  test("project-relevance: package.json root marker wins without walking", async () => {
    writeFileSync(join(tempProject, "package.json"), "{}");
    const result = runAutoInstall(tempProject, defaultConfig({ graceDays: 365 }), fakeFetch());
    await result.installsComplete;

    const tsSkipped = result.skipped.find((s) => s.id === "typescript");
    expect(tsSkipped?.reason).toContain("grace");
  });

  test("project-relevance: pyproject.toml triggers Python discovery", async () => {
    writeFileSync(join(tempProject, "pyproject.toml"), "[tool.poetry]\nname = 'x'");
    const result = await runAutoInstall(tempProject, defaultConfig(), fakeFetch());
    const pythonSkipped = result.skipped.find((s) => s.id === "python");
    expect(pythonSkipped).toBeUndefined();
  });

  test("project-relevance: extension-only file triggers discovery", async () => {
    writeFileSync(join(tempProject, "config.yaml"), "key: value");
    const result = await runAutoInstall(tempProject, defaultConfig(), fakeFetch());
    const yamlSkipped = result.skipped.find((s) => s.id === "yaml");
    expect(yamlSkipped).toBeUndefined();
  });

  test("project-relevance: bounded walk finds nested TypeScript files", async () => {
    const srcDir = join(tempProject, "packages", "app", "src");
    mkdirSync(srcDir, { recursive: true });
    writeFileSync(join(srcDir, "main.ts"), "export const value = 1;\n");

    const result = runAutoInstall(tempProject, defaultConfig({ graceDays: 365 }), fakeFetch());
    await result.installsComplete;

    const tsSkipped = result.skipped.find((s) => s.id === "typescript");
    expect(tsSkipped?.reason).toContain("grace");
  });

  test("project-relevance: bounded walk ignores noise directories", async () => {
    const noiseDir = join(tempProject, "node_modules", "dependency");
    mkdirSync(noiseDir, { recursive: true });
    writeFileSync(join(noiseDir, "big.ts"), "export const vendored = true;\n");

    const result = runAutoInstall(tempProject, defaultConfig(), fakeFetch());

    const tsSkipped = result.skipped.find((s) => s.id === "typescript");
    expect(tsSkipped?.reason).toBe("not relevant to project");
    expect(result.installsStarted).toBe(0);
  });

  test("project-relevance: Dockerfile root marker triggers discovery", async () => {
    writeFileSync(join(tempProject, "Dockerfile"), "FROM node:20");
    const result = await runAutoInstall(tempProject, defaultConfig(), fakeFetch());
    const dockerSkipped = result.skipped.find((s) => s.id === "dockerfile");
    expect(dockerSkipped).toBeUndefined();
  });

  test("biome.json triggers biome-only discovery", async () => {
    writeFileSync(join(tempProject, "biome.json"), "{}");
    const result = await runAutoInstall(tempProject, defaultConfig(), fakeFetch());
    const biomeSkipped = result.skipped.find((s) => s.id === "biome");
    expect(biomeSkipped).toBeUndefined();
  });

  test("graceDays high enough to block all versions does not start install when nothing is cached", async () => {
    writeFileSync(join(tempProject, "package.json"), "{}");
    // fakeFetch returns version published 60 days ago; require 365 days grace.
    const result = runAutoInstall(tempProject, defaultConfig({ graceDays: 365 }), fakeFetch());
    // installsStarted is initially the kicked-off count; await installsComplete
    // and then read again to see the final post-decrement count for skipped servers.
    await result.installsComplete;
    expect(result.installsStarted).toBe(0);
    const tsSkip = result.skipped.find((s) => s.id === "typescript");
    expect(tsSkip).toBeDefined();
    expect(tsSkip?.reason).toContain("grace");
  });

  test("graceDays high but a version is already installed: keep it, don't reinstall", async () => {
    writeFileSync(join(tempProject, "package.json"), "{}");
    fakeInstalled("typescript-language-server", "typescript-language-server");

    const result = runAutoInstall(tempProject, defaultConfig({ graceDays: 365 }), fakeFetch());
    await result.installsComplete;
    expect(result.installsStarted).toBe(0);
    expect(result.cachedBinDirs).toHaveLength(1);
    // Skipped reason should reflect "kept existing install" — not just "blocked".
    const tsSkip = result.skipped.find((s) => s.id === "typescript");
    expect(tsSkip?.reason).toContain("existing");
  });

  test("user pin via lsp.versions bypasses the grace filter", async () => {
    writeFileSync(join(tempProject, "package.json"), "{}");
    const result = runAutoInstall(
      tempProject,
      defaultConfig({
        // Even with grace=365 (would block), user pin overrides.
        graceDays: 365,
        versions: { "typescript-language-server": "9.9.9" },
      }),
      fakeFetch(),
    );
    // Synchronously the install was kicked off (installsStarted >= 1 before the
    // promise settles). The actual `npm install` will fail (no network in test) but
    // that is logged, not surfaced here.
    expect(result.installsStarted).toBeGreaterThan(0);
  });

  test("registry probe network failure still returns cached paths", async () => {
    fakeInstalled("yaml-language-server", "yaml-language-server");
    writeFileSync(join(tempProject, "config.yml"), "");

    const failingFetch = (async () => {
      throw new Error("network down");
    }) as typeof fetch;

    const result = runAutoInstall(tempProject, defaultConfig(), failingFetch);
    await result.installsComplete;
    expect(result.cachedBinDirs).toHaveLength(1);
    expect(result.installsStarted).toBe(0);
  });

  test("runInstall uses npm for Pi and unreferences spawned children", () => {
    const source = readFileSync(new URL("../lsp-auto-install.ts", import.meta.url), "utf8");
    // Node's child_process.spawn on Windows does not auto-resolve `.cmd`
    // shims, and npm ships as `npm.cmd` there. Assert the platform branch
    // + the spawn invocation separately so the test survives formatter
    // reflows of the source.
    expect(source).toMatch(
      /process\.platform\s*===\s*["']win32["']\s*\?\s*["']npm\.cmd["']\s*:\s*["']npm["']/,
    );
    expect(source).toMatch(
      /spawn\(\s*\w+\s*,\s*\[\s*["']install["']\s*,\s*["']--no-save["']\s*,\s*["']--ignore-scripts["']\s*,\s*["']--silent["']\s*,\s*target\s*\]/,
    );
    expect(source).not.toContain('spawn("bun"');
    expect(source).toContain("child.unref()");
  });
});
