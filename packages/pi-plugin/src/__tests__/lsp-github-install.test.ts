/**
 * Orchestrator tests for `lsp-github-install.ts`.
 *
 * We test the high-level decisions: project relevance gating, disabled
 * filter, auto_install: false short-circuit, and the cache-detection
 * paths that surface `cachedBinDirs` to the caller. The actual
 * download+extract is exercised end-to-end in the live integration
 * tests (Stage 5+) — unit tests here mock fetch and avoid spawning
 * real processes.
 */

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { acquireEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import {
  discoverRelevantGithubServers,
  ghBinaryPath,
  isGithubInstalled,
  runGithubAutoInstall,
} from "../lsp-github-install.js";
import { findGithubServerById } from "../lsp-github-table.js";

let tmpRoot = "";
let projectRoot = "";
let releaseEnv: (() => void) | undefined;

beforeEach(async () => {
  tmpRoot = mkdtempSync(join(tmpdir(), "aft-gh-test-"));
  projectRoot = mkdtempSync(join(tmpdir(), "aft-gh-project-"));
  releaseEnv = await acquireEnv({ AFT_CACHE_DIR: tmpRoot });
});

afterEach(() => {
  releaseEnv?.();
  releaseEnv = undefined;
  try {
    rmSync(tmpRoot, { recursive: true, force: true });
    rmSync(projectRoot, { recursive: true, force: true });
  } catch {
    // ignore
  }
});

describe("discoverRelevantGithubServers", () => {
  test("returns clangd for a .c file", () => {
    writeFileSync(join(projectRoot, "main.c"), "int main(){}");
    const ids = discoverRelevantGithubServers(projectRoot);
    expect(ids.has("clangd")).toBe(true);
  });

  test("walks nested source directories", () => {
    const srcDir = join(projectRoot, "packages", "native", "src");
    mkdirSync(srcDir, { recursive: true });
    writeFileSync(join(srcDir, "main.c"), "int main(){}");

    const ids = discoverRelevantGithubServers(projectRoot);

    expect(ids.has("clangd")).toBe(true);
  });

  test("ignores files in noise directories", () => {
    const noiseDir = join(projectRoot, "node_modules", "native");
    mkdirSync(noiseDir, { recursive: true });
    writeFileSync(join(noiseDir, "main.c"), "int main(){}");

    const ids = discoverRelevantGithubServers(projectRoot);

    expect(ids.has("clangd")).toBe(false);
  });

  test("returns lua-ls for a .lua file", () => {
    writeFileSync(join(projectRoot, "init.lua"), "");
    const ids = discoverRelevantGithubServers(projectRoot);
    expect(ids.has("lua-ls")).toBe(true);
  });

  test("returns zls for a .zig file", () => {
    writeFileSync(join(projectRoot, "build.zig"), "");
    const ids = discoverRelevantGithubServers(projectRoot);
    expect(ids.has("zls")).toBe(true);
  });

  test("returns tinymist for a .typ file", () => {
    writeFileSync(join(projectRoot, "doc.typ"), "");
    const ids = discoverRelevantGithubServers(projectRoot);
    expect(ids.has("tinymist")).toBe(true);
  });

  test("returns texlab for a .tex file", () => {
    writeFileSync(join(projectRoot, "paper.tex"), "");
    const ids = discoverRelevantGithubServers(projectRoot);
    expect(ids.has("texlab")).toBe(true);
  });

  test("ignores unrelated files", () => {
    writeFileSync(join(projectRoot, "README.md"), "");
    writeFileSync(join(projectRoot, "package.json"), "{}");
    const ids = discoverRelevantGithubServers(projectRoot);
    expect(ids.size).toBe(0);
  });

  test("returns empty for unreadable project root", () => {
    const ids = discoverRelevantGithubServers(join(projectRoot, "nonexistent"));
    expect(ids.size).toBe(0);
  });
});

describe("isGithubInstalled", () => {
  test("false when binary file missing", () => {
    const clangd = findGithubServerById("clangd");
    if (!clangd) throw new Error("clangd missing");
    expect(isGithubInstalled(clangd, "linux")).toBe(false);
  });

  test("true when binary file exists at expected path", () => {
    const clangd = findGithubServerById("clangd");
    if (!clangd) throw new Error("clangd missing");
    const bin = ghBinaryPath(clangd, "linux");
    mkdirSync(join(bin, ".."), { recursive: true });
    writeFileSync(bin, "fake");
    expect(isGithubInstalled(clangd, "linux")).toBe(true);
  });

  test("true on Windows when .exe binary exists at expected path", () => {
    const clangd = findGithubServerById("clangd");
    if (!clangd) throw new Error("clangd missing");
    const bin = ghBinaryPath(clangd, "win32");
    mkdirSync(join(bin, ".."), { recursive: true });
    writeFileSync(bin, "fake");

    expect(bin.endsWith("clangd.exe")).toBe(true);
    expect(isGithubInstalled(clangd, "win32")).toBe(true);
  });
});

describe("runGithubAutoInstall", () => {
  test("auto_install: false skips everything but still surfaces cached bin dirs", async () => {
    const clangd = findGithubServerById("clangd");
    if (!clangd) throw new Error("clangd missing");
    const platform = process.platform === "win32" ? "win32" : process.platform;
    if (platform !== "darwin" && platform !== "linux" && platform !== "win32") {
      // Skip on unsupported hosts.
      return;
    }
    const bin = ghBinaryPath(clangd, platform as "darwin" | "linux" | "win32");
    mkdirSync(join(bin, ".."), { recursive: true });
    const binContent = "fake";
    writeFileSync(bin, binContent);
    // Lane F's TOFU validation needs a valid `.aft-installed` whose
    // recorded sha256 matches the binary, otherwise the cached entry is
    // quarantined and won't appear in cachedBinDirs.
    const sha256 = createHash("sha256").update(binContent).digest("hex");
    // ghPackageDir is not exported, but the layout is <pkg>/bin/<binary>,
    // so dirname twice from the binary path gives us the package dir.
    const packageDir = join(bin, "..", "..");
    writeFileSync(
      join(packageDir, ".aft-installed"),
      JSON.stringify({ version: "21.1.0", installedAt: "now", sha256 }),
    );

    const fakeFetch = (async () => {
      throw new Error("should not be called when auto_install is false");
    }) as unknown as typeof fetch;

    const result = await runGithubAutoInstall(
      new Set(["clangd"]),
      {
        autoInstall: false,
        graceDays: 7,
        versions: {},
        disabled: new Set(),
      },
      fakeFetch,
    );
    expect(result.cachedBinDirs.length).toBeGreaterThan(0);
    expect(result.installsStarted).toBe(0);
    expect(
      result.skipped.some((s) => s.id === "clangd" && s.reason === "auto_install: false"),
    ).toBe(true);
  });

  test("disabled servers skipped with reason", async () => {
    const fakeFetch = (async () => {
      throw new Error("should not be called when server disabled");
    }) as unknown as typeof fetch;

    const result = await runGithubAutoInstall(
      new Set(["clangd", "zls"]),
      {
        autoInstall: true,
        graceDays: 7,
        versions: {},
        disabled: new Set(["clangd", "zls"]),
      },
      fakeFetch,
    );
    expect(result.installsStarted).toBe(0);
    expect(result.skipped.some((s) => s.id === "clangd" && s.reason === "disabled by config")).toBe(
      true,
    );
    expect(result.skipped.some((s) => s.id === "zls" && s.reason === "disabled by config")).toBe(
      true,
    );
  });

  test("not-relevant servers skipped (relevantServers set is empty)", async () => {
    const fakeFetch = (async () => {
      throw new Error("should not be called");
    }) as unknown as typeof fetch;

    const result = await runGithubAutoInstall(
      new Set(), // nothing relevant
      {
        autoInstall: true,
        graceDays: 7,
        versions: {},
        disabled: new Set(),
      },
      fakeFetch,
    );
    expect(result.installsStarted).toBe(0);
    // Every spec should be in skipped with "not relevant to project".
    const reasons = new Set(result.skipped.map((s) => s.reason));
    expect(reasons.has("not relevant to project")).toBe(true);
  });

  test("blockedByGrace from probe surfaces as skip with grace reason", async () => {
    // Fake fetch returns a release that's only 2 days old.
    const recent = new Date(Date.now() - 2 * 24 * 60 * 60 * 1000).toISOString();
    const fakeFetch = (async () =>
      new Response(
        JSON.stringify([
          {
            tag_name: "v9.9.9",
            published_at: recent,
            draft: false,
            prerelease: false,
            assets: [],
          },
        ]),
        { status: 200 },
      )) as unknown as typeof fetch;

    const result = runGithubAutoInstall(
      new Set(["clangd"]),
      {
        autoInstall: true,
        graceDays: 7,
        versions: {},
        disabled: new Set(),
      },
      fakeFetch,
    );
    await result.installsComplete;
    expect(result.installsStarted).toBe(0);
    const clangdSkip = result.skipped.find((s) => s.id === "clangd");
    expect(clangdSkip).toBeDefined();
    expect(clangdSkip?.reason).toContain("grace window");
  });

  test("registry probe failure → skip with probe-failed reason and no install", async () => {
    const fakeFetch = (async () =>
      new Response("Internal", { status: 500 })) as unknown as typeof fetch;

    const result = runGithubAutoInstall(
      new Set(["clangd"]),
      {
        autoInstall: true,
        graceDays: 7,
        versions: {},
        disabled: new Set(),
      },
      fakeFetch,
    );
    await result.installsComplete;
    expect(result.installsStarted).toBe(0);
    const clangdSkip = result.skipped.find((s) => s.id === "clangd");
    expect(clangdSkip).toBeDefined();
    expect(clangdSkip?.reason).toContain("probe");
  });
});
