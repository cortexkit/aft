import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { acquireEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import {
  type AutoInstallConfig,
  ensureInstallAnchor,
  pushLspPathsAfterAutoInstall,
  resolveTargetVersion,
  runAutoInstall,
} from "../lsp-auto-install";
import { lspBinaryPath, writeInstalledMeta, writeVersionCheck } from "../lsp-cache";
import { findNpmServerById } from "../lsp-npm-table";

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
function fakeInstalled(npmPackage: string, binary: string, version = "1.0.0"): string {
  const path = lspBinaryPath(npmPackage, binary);
  mkdirSync(join(path, ".."), { recursive: true });
  const content = "#!/bin/sh\nexit 0\n";
  writeFileSync(path, content);
  const sha256 = createHash("sha256").update(content).digest("hex");
  // Version is arbitrary but must pass isSafeVersion (semver-ish).
  writeInstalledMeta(npmPackage, version, sha256);
  return path;
}

describe("runAutoInstall", () => {
  test("pushes refreshed paths to live bridges and future spawns", async () => {
    const overrides: Record<string, unknown> = {};
    const reconfigures: Array<{ root: string; params: Record<string, unknown> }> = [];
    const pool = {
      setConfigureOverride(key: string, value: unknown) {
        overrides[key] = value;
      },
      async reconfigure(root: string, params: Record<string, unknown>) {
        reconfigures.push({ root, params });
      },
    };

    await pushLspPathsAfterAutoInstall(pool, tempProject, ["/cache/a", "/cache/a", "/cache/b"]);

    expect(overrides.lsp_paths_extra).toEqual(["/cache/a", "/cache/b"]);
    expect(reconfigures).toEqual([
      {
        root: tempProject,
        params: { lsp_paths_extra: ["/cache/a", "/cache/b"] },
      },
    ]);
  });

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

  test("surfaces the separately cached TypeScript SDK for fresh worktrees", async () => {
    // The SDK entry only accepts the 5.x line, so the fixture uses a 5.x version.
    fakeInstalled("typescript", "tsserver", "5.9.3");
    const result = await runAutoInstall(
      tempProject,
      defaultConfig({ autoInstall: false }),
      fakeFetch(),
    );
    expect(result.cachedBinDirs).toHaveLength(1);
    expect(result.cachedBinDirs[0]).toContain("typescript");
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
    expect(disabledEntries.map((s) => s.id)).toContain("typescript-sdk");
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
    expect(result.installingBinaries).not.toContain("typescript-language-server");
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
    expect(result.installingBinaries).not.toContain("yaml-language-server");
  });

  test("runInstall uses resolved npm for Pi and unreferences spawned children", () => {
    const source = readFileSync(new URL("../lsp-auto-install.ts", import.meta.url), "utf8");
    // npm must be resolved beyond a GUI-stripped PATH, then converted to a
    // platform-safe invocation (npm.cmd requires cmd.exe on Windows).
    expect(source).toMatch(/resolveNpm\(\)/);
    expect(source).toContain("await lspNpmInstallInvocation(npm, target, before)");
    expect(source).toMatch(/spawn\(invocation\.command, invocation\.args,/);
    expect(source).toContain("npmSpawnEnv(npm)");
    expect(source).toContain("terminate: terminateNpmProcessTree");
    expect(source).toContain("runtime.terminate(child, invocation)");
    expect(source).toMatch(/terminationPromise\.then\(\s*finishAfterConfirmedTermination,/);
    expect(source).toContain("const completedSuccessfully = childCompletedSuccessfully()");
    expect(source).toContain("finish(completedSuccessfully)");
    expect(source).toContain("if (settled || terminationPromise) return;");
    expect(source).toContain("quarantining retries for this session");
    expect(source).toContain("quarantinedNpmInstalls.has(spec.npm)");
    expect(source).toContain("installLock.retain()");
    expect(source).toContain("if (child.pid === undefined)");
    expect(source).toContain("if (terminationPromise && child.pid !== undefined) return;");
    expect(source).not.toContain('child.kill("SIGTERM")');
    expect(source).toContain("finish(false)");
    // The real-spawn policy tests assert the shared helper's final npm argv.
    expect(source).toContain(
      "lspPublicationCutoff(config.graceDays, Boolean(config.versions[spec.npm]))",
    );
    expect(source).not.toContain('spawn("bun"');
    expect(source).toContain("child.unref()");
  });

  // GitHub #92: `npm install --no-save` with no package.json in the cache dir
  // walks UP the tree and, if an ancestor has a package.json, installs into
  // THAT package's node_modules instead — leaving our cache dir empty while
  // exiting 0 (silent failure). ensureInstallAnchor writes the anchoring stub.
  describe("ensureInstallAnchor (GitHub #92)", () => {
    test("writes a private package.json when none exists", () => {
      const dir = mkdtempSync(join(tmpdir(), "aft-anchor-"));
      try {
        expect(existsSync(join(dir, "package.json"))).toBe(false);
        ensureInstallAnchor(dir);
        const pkg = JSON.parse(readFileSync(join(dir, "package.json"), "utf8"));
        expect(pkg).toMatchObject({ name: "aft-lsp-cache", private: true });
      } finally {
        rmSync(dir, { recursive: true, force: true });
      }
    });

    test("is idempotent and does not clobber an existing package.json", () => {
      const dir = mkdtempSync(join(tmpdir(), "aft-anchor-"));
      try {
        const existing = `${JSON.stringify({ name: "user-project", version: "9.9.9" })}\n`;
        writeFileSync(join(dir, "package.json"), existing);
        ensureInstallAnchor(dir);
        expect(readFileSync(join(dir, "package.json"), "utf8")).toBe(existing);
      } finally {
        rmSync(dir, { recursive: true, force: true });
      }
    });

    test("does not throw when the directory is missing", () => {
      const missing = join(tmpdir(), `aft-anchor-missing-${Date.now()}`, "nope");
      expect(() => ensureInstallAnchor(missing)).not.toThrow();
    });
  });
});

describe("typescript-sdk version cap", () => {
  /** Registry response whose releases were published `days` ago. */
  function registryFetch(releases: Record<string, number>): typeof fetch {
    return (async () => {
      const now = Date.now();
      const time: Record<string, string> = {};
      for (const [version, days] of Object.entries(releases)) {
        time[version] = isoDaysAgo(now, days);
      }
      return {
        ok: true,
        status: 200,
        async json() {
          return { time };
        },
      } as Response;
    }) as typeof fetch;
  }

  function sdkSpec() {
    const spec = findNpmServerById("typescript-sdk");
    if (!spec) throw new Error("typescript-sdk entry missing");
    return spec;
  }

  test("a fresh cached 7.x decision is ignored and the newest 5.x is chosen", async () => {
    // Before the cap, the version check cache recorded 7.0.2 as the latest
    // eligible release. Consuming it would keep reinstalling the native
    // compiler, which has no tsserver.js.
    writeVersionCheck("typescript", "7.0.2");
    const { version } = await resolveTargetVersion(
      sdkSpec(),
      defaultConfig(),
      registryFetch({ "5.8.3": 200, "5.9.3": 120, "6.0.2": 60, "7.0.2": 30 }),
    );
    // 5.9.3 differs from an installed 7.0.2, so the installer reinstalls.
    expect(version).toBe("5.9.3");
  });

  test("a failed probe does not fall back to a cached 7.x decision", async () => {
    writeVersionCheck("typescript", "7.0.2");
    const failingFetch = (async () => {
      throw new Error("network down");
    }) as typeof fetch;
    const { version } = await resolveTargetVersion(
      sdkSpec(),
      defaultConfig({ graceDays: 0 }),
      failingFetch,
    );
    expect(version).toBeNull();
  });

  test("a user pin still wins over the cap", async () => {
    const { version, pinned } = await resolveTargetVersion(
      sdkSpec(),
      defaultConfig({ versions: { typescript: "7.0.2" } }),
      registryFetch({ "5.9.3": 120 }),
    );
    expect(version).toBe("7.0.2");
    expect(pinned).toBe(true);
  });

  test("a cached 7.x install is neither surfaced nor kept as the existing install", async () => {
    writeFileSync(join(tempProject, "tsconfig.json"), "{}");
    fakeInstalled("typescript", "tsserver", "7.0.2");
    // A 365-day grace window blocks every release, so no npm install runs.
    const result = runAutoInstall(
      tempProject,
      defaultConfig({ graceDays: 365 }),
      registryFetch({ "5.9.3": 120, "7.0.2": 30 }),
    );
    await result.installsComplete;
    expect(
      result.cachedBinDirs.some((dir) => dir.includes(join("typescript", "node_modules"))),
    ).toBe(false);
    const sdkSkip = result.skipped.find((s) => s.id === "typescript-sdk");
    expect(sdkSkip?.reason).toContain("7.0.2 is outside the supported 5.x line");
  });

  test("a cached 5.x install is still kept when no newer release is eligible", async () => {
    writeFileSync(join(tempProject, "tsconfig.json"), "{}");
    fakeInstalled("typescript", "tsserver", "5.9.3");
    const result = runAutoInstall(
      tempProject,
      defaultConfig({ graceDays: 365 }),
      registryFetch({ "5.9.3": 120, "7.0.2": 30 }),
    );
    await result.installsComplete;
    expect(
      result.cachedBinDirs.some((dir) => dir.includes(join("typescript", "node_modules"))),
    ).toBe(true);
    const sdkSkip = result.skipped.find((s) => s.id === "typescript-sdk");
    expect(sdkSkip?.reason).toBe("kept existing install");
  });
});
