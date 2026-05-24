/// <reference path="../bun-test.d.ts" />

import { afterEach, describe, expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { PLATFORM_ASSET_MAP } from "@cortexkit/aft-bridge";

const packageRoot = fileURLToPath(new URL("../../", import.meta.url));
const tempRoots = new Set<string>();
const currentPlatformKey = `${process.platform}-${process.arch}`;
const currentAssetName = PLATFORM_ASSET_MAP[currentPlatformKey];
const binaryName = process.platform === "win32" ? "aft.exe" : "aft";

function createCacheRoot() {
  const root = mkdtempSync(join(tmpdir(), "aft-downloader-tests-"));
  tempRoots.add(root);
  return root;
}

function runDownloaderScript(script: string, env: Record<string, string> = {}) {
  const result = spawnSync(process.execPath, ["-e", script], {
    cwd: packageRoot,
    env: { ...process.env, AFT_LOG_STDERR: "1", ...env },
    encoding: "utf8",
  });

  expect(result.error).toBeUndefined();
  expect(result.status).toBe(0);

  return {
    stdout: result.stdout.trim(),
    stderr: result.stderr.trim(),
  };
}

afterEach(() => {
  for (const root of tempRoots) {
    rmSync(root, { recursive: true, force: true });
  }
  tempRoots.clear();
});

describe("downloadBinary error paths", () => {
  test("returns null for unsupported platforms", () => {
    const result = runDownloaderScript(`
      Object.defineProperty(process, "platform", { value: "plan9" });
      Object.defineProperty(process, "arch", { value: "x64" });
      const { downloadBinary } = await import("@cortexkit/aft-bridge");
      console.log(String(await downloadBinary("v1.2.3")));
    `);

    expect(result.stdout).toBe("null");
    // No host logger registered in subprocess — falls back to [aft-bridge] prefix
    expect(result.stderr).toContain("Unsupported platform: plan9-x64");
  });

  test("returns null and logs HTTP download failures", () => {
    if (!currentAssetName) throw new Error(`Unsupported test platform: ${currentPlatformKey}`);
    const cacheRoot = createCacheRoot();
    const result = runDownloaderScript(
      `
        globalThis.fetch = async (url) => {
          if (String(url).endsWith("checksums.sha256")) {
            return new Response("", { status: 404, statusText: "Not Found" });
          }
          return new Response("bad", { status: 502, statusText: "Bad Gateway" });
        };
        const { downloadBinary } = await import("@cortexkit/aft-bridge");
        console.log(String(await downloadBinary("v9.9.9")));
      `,
      { XDG_CACHE_HOME: cacheRoot },
    );

    expect(result.stdout).toBe("null");
    expect(result.stderr).toContain(
      `Failed to download AFT binary: HTTP 502: Bad Gateway (https://github.com/cortexkit/aft/releases/download/v9.9.9/${currentAssetName})`,
    );
  });

  test("returns null when checksum verification fails", () => {
    if (!currentAssetName) throw new Error(`Unsupported test platform: ${currentPlatformKey}`);
    const cacheRoot = createCacheRoot();
    const wrongHash = "0".repeat(64);
    const result = runDownloaderScript(
      `
        globalThis.fetch = async (url) => {
          if (String(url).endsWith("checksums.sha256")) {
            return new Response(${JSON.stringify("")} + ${JSON.stringify(wrongHash)} + "  ${currentAssetName}\\n", { status: 200 });
          }
          return new Response("binary payload", { status: 200 });
        };
        const { downloadBinary } = await import("@cortexkit/aft-bridge");
        console.log(String(await downloadBinary("v1.0.0")));
      `,
      { XDG_CACHE_HOME: cacheRoot },
    );

    expect(result.stdout).toBe("null");
    expect(result.stderr).toContain(
      `Checksum mismatch for ${currentAssetName}: expected ${wrongHash}`,
    );
    expect(existsSync(join(cacheRoot, "aft", "bin", binaryName))).toBe(false);
  });

  test("returns null when the checksum file is unavailable (security requirement)", () => {
    if (!currentAssetName) throw new Error(`Unsupported test platform: ${currentPlatformKey}`);
    const cacheRoot = createCacheRoot();
    const result = runDownloaderScript(
      `
        globalThis.fetch = async (url) => {
          if (String(url).endsWith("checksums.sha256")) {
            return new Response("missing", { status: 404, statusText: "Not Found" });
          }
          return new Response("binary payload", { status: 200 });
        };
        const { downloadBinary } = await import("@cortexkit/aft-bridge");
        console.log(String(await downloadBinary("v2.0.0")));
      `,
      { XDG_CACHE_HOME: cacheRoot },
    );

    expect(result.stdout).toBe("null");
    expect(existsSync(join(cacheRoot, "aft", "bin", "v2.0.0", binaryName))).toBe(false);
    expect(result.stderr).toContain(
      "Checksum verification failed: no checksums.sha256 found for v2.0.0",
    );
    expect(result.stderr).toContain("Binary download aborted for security reasons");
  });

  test("returns null when checksum file has no entry for the asset (security requirement)", () => {
    if (!currentAssetName) throw new Error(`Unsupported test platform: ${currentPlatformKey}`);
    const cacheRoot = createCacheRoot();
    const result = runDownloaderScript(
      `
        globalThis.fetch = async (url) => {
          if (String(url).endsWith("checksums.sha256")) {
            return new Response("not-a-checksum\\n12345 missing-entry\\n", { status: 200 });
          }
          return new Response("binary payload", { status: 200 });
        };
        const { downloadBinary } = await import("@cortexkit/aft-bridge");
        console.log(String(await downloadBinary("v3.0.0")));
      `,
      { XDG_CACHE_HOME: cacheRoot },
    );

    expect(result.stdout).toBe("null");
    expect(existsSync(join(cacheRoot, "aft", "bin", "v3.0.0", binaryName))).toBe(false);
    expect(result.stderr).toContain(
      `Checksum verification failed: checksums.sha256 found but no entry for ${currentAssetName}`,
    );
    expect(result.stderr).toContain("Binary download aborted for security reasons");
  });
});

describe("downloadBinary tag normalization (regression for v0.25.1 404 bug)", () => {
  // Background: prior to v0.25.2, `findBinary` → `ensureBinary("0.25.1")` (no `v`
  // prefix) constructed `releases/download/0.25.1/<asset>` which 404'd because
  // GitHub release tags always have the `v` prefix. The cache layout was also
  // split between `<cache>/0.25.1/` and `<cache>/v0.25.1/`. Normalization now
  // happens at the boundary so any caller convention works.

  test("constructs v-prefixed URL when caller passes bare version (no leading v)", () => {
    if (!currentAssetName) throw new Error(`Unsupported test platform: ${currentPlatformKey}`);
    const cacheRoot = createCacheRoot();
    const result = runDownloaderScript(
      `
        const seenUrls = [];
        globalThis.fetch = async (url) => {
          seenUrls.push(String(url));
          // Return 502 so the download fails fast but URL gets captured first.
          return new Response("nope", { status: 502, statusText: "Bad Gateway" });
        };
        const { downloadBinary } = await import("@cortexkit/aft-bridge");
        await downloadBinary("0.25.1");
        console.log(JSON.stringify(seenUrls));
      `,
      { XDG_CACHE_HOME: cacheRoot },
    );

    const urls = JSON.parse(result.stdout) as string[];
    expect(urls).toContain(
      `https://github.com/cortexkit/aft/releases/download/v0.25.1/${currentAssetName}`,
    );
    expect(urls).toContain(
      "https://github.com/cortexkit/aft/releases/download/v0.25.1/checksums.sha256",
    );
    // Critically: NO URL should reference the bare "0.25.1" without the prefix.
    for (const url of urls) {
      expect(url).not.toContain("/releases/download/0.25.1/");
    }
  });

  test("caches under v-prefixed dir when caller passes bare version", () => {
    if (!currentAssetName) throw new Error(`Unsupported test platform: ${currentPlatformKey}`);
    const cacheRoot = createCacheRoot();
    runDownloaderScript(
      `
        globalThis.fetch = async (url) => {
          return new Response("nope", { status: 502, statusText: "Bad Gateway" });
        };
        const { downloadBinary } = await import("@cortexkit/aft-bridge");
        await downloadBinary("0.42.0");
      `,
      { XDG_CACHE_HOME: cacheRoot },
    );

    // The (failed) download attempt should have created the v-prefixed cache
    // directory, not the bare-version one. Cleanup of partial download .tmp
    // files leaves an empty directory; we only assert the dir structure.
    expect(existsSync(join(cacheRoot, "aft", "bin", "v0.42.0"))).toBe(true);
    expect(existsSync(join(cacheRoot, "aft", "bin", "0.42.0"))).toBe(false);
  });

  test("ensureBinary normalizes bare version for cache hit", () => {
    if (!currentAssetName) throw new Error(`Unsupported test platform: ${currentPlatformKey}`);
    const cacheRoot = createCacheRoot();

    // Pre-seed the cache at the v-prefixed path (where downloadBinary would
    // have written it). ensureBinary("0.7.7") should find this cached file
    // because it normalizes before lookup.
    const cachedDir = join(cacheRoot, "aft", "bin", "v0.7.7");
    const cachedPath = join(cachedDir, binaryName);
    mkdtempSync; // keep import used
    spawnSync("mkdir", ["-p", cachedDir]);
    spawnSync("touch", [cachedPath]);

    const result = runDownloaderScript(
      `
        // Fail any unexpected network attempt so we can prove the cache hit.
        globalThis.fetch = async () => {
          throw new Error("should not fetch — cache should have been hit");
        };
        const { ensureBinary } = await import("@cortexkit/aft-bridge");
        const found = await ensureBinary("0.7.7");
        console.log(found ?? "null");
      `,
      { XDG_CACHE_HOME: cacheRoot },
    );

    expect(result.stdout).toBe(cachedPath);
  });
});
