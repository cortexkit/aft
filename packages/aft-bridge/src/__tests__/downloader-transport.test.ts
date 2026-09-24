/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import {
  chmodSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  utimesSync,
  writeFileSync,
} from "node:fs";
import { hostname, tmpdir } from "node:os";
import { join } from "node:path";

import { isTrustedCachedBinary, readBinaryIdentity } from "../binary-identity.js";
import { PLATFORM_ARCH_MAP, PLATFORM_ASSET_MAP } from "../platform.js";
import { __setOffThreadVersionProbeForTests } from "../version-probe.js";
import { acquireEnv } from "./test-utils/env-guard.js";

const shellFixtureSkipReason =
  process.platform === "win32" ? "POSIX shell fixture is unavailable on Windows" : "";

function shellFixtureAvailable(): boolean {
  if (!shellFixtureSkipReason) return true;
  if (process.env.CI === "true") throw new Error(shellFixtureSkipReason);
  return false;
}

describe("downloadBinary hardened transport", () => {
  let tmpDir: string;
  let releaseEnv: (() => void) | undefined;
  let originalFetch: typeof fetch;

  beforeEach(async () => {
    tmpDir = mkdtempSync(join(tmpdir(), "aft-download-test-"));
    // The shared cache resolver reads AFT_CACHE_DIR first, then the
    // platform-specific cache environment variables.
    const cacheEnv =
      process.platform === "win32" ? { LOCALAPPDATA: tmpDir } : { XDG_CACHE_HOME: tmpDir };
    // AFT_CACHE_DIR outranks the platform vars in the resolver, and CI exports
    // one ambiently; clear it so the tmpDir sandbox wins.
    releaseEnv = await acquireEnv({ AFT_CACHE_DIR: undefined, ...cacheEnv });
    originalFetch = globalThis.fetch;
  });

  afterEach(() => {
    releaseEnv?.();
    releaseEnv = undefined;
    globalThis.fetch = originalFetch;
    rmSync(tmpDir, { recursive: true, force: true });
  });

  function currentAssetName(): string {
    const platformKey = PLATFORM_ARCH_MAP[process.platform]?.[process.arch];
    const assetName = platformKey ? PLATFORM_ASSET_MAP[platformKey] : undefined;
    if (!assetName)
      throw new Error(`unsupported test platform ${process.platform}-${process.arch}`);
    return assetName;
  }

  test("dedupes concurrent same-version downloads and writes one final binary", async () => {
    if (!shellFixtureAvailable()) return;
    const { downloadBinary, getBinaryName } = await import(
      `../downloader.js?transport-dedupe-${Date.now()}`
    );
    const assetName = currentAssetName();
    const payload = Buffer.from("#!/bin/sh\necho aft 1.2.3\n");
    const sha256 = createHash("sha256").update(payload).digest("hex");
    let binaryFetches = 0;

    globalThis.fetch = (async (url: string | URL | Request) => {
      const rawUrl = String(url);
      if (rawUrl.endsWith("checksums.sha256")) {
        return new Response(`${sha256}  ${assetName}\n`, { status: 200 });
      }
      binaryFetches += 1;
      return new Response(payload, {
        status: 200,
        headers: { "content-length": String(payload.byteLength) },
      });
    }) as typeof fetch;

    const [first, second] = await Promise.all([downloadBinary("v1.2.3"), downloadBinary("1.2.3")]);

    const expectedPath = join(tmpDir, "aft", "bin", "v1.2.3", getBinaryName());
    expect(first).toBe(expectedPath);
    expect(second).toBe(expectedPath);
    expect(binaryFetches).toBe(1);
    expect(existsSync(expectedPath)).toBe(true);
    expect(
      readdirSync(join(tmpDir, "aft", "bin", "v1.2.3")).filter((name) => name.includes(".tmp")),
    ).toEqual([]);
    // The checksum-verified download records its identity once in place, so
    // later lookups trust it with a stat instead of running it.
    expect(isTrustedCachedBinary(expectedPath, "1.2.3")).toBe(true);
    expect(readBinaryIdentity(expectedPath)?.sha256).toBe(sha256);
  });

  test("a cached download with a matching sidecar is reused without probing its version", async () => {
    const { ensureBinary, getBinaryName } = await import(
      `../downloader.js?sidecar-reuse-${Date.now()}`
    );
    const assetName = currentAssetName();
    const payload = Buffer.from("release bytes 1.2.6");
    const sha256 = createHash("sha256").update(payload).digest("hex");
    let binaryFetches = 0;
    globalThis.fetch = (async (url: string | URL | Request) => {
      if (String(url).endsWith("checksums.sha256")) {
        return new Response(`${sha256}  ${assetName}\n`, { status: 200 });
      }
      binaryFetches += 1;
      return new Response(payload, { status: 200 });
    }) as typeof fetch;
    const probes: string[] = [];
    __setOffThreadVersionProbeForTests(async (path) => {
      probes.push(path);
      return null;
    });
    try {
      const expectedPath = join(tmpDir, "aft", "bin", "v1.2.6", getBinaryName());
      await expect(ensureBinary("v1.2.6")).resolves.toBe(expectedPath);
      await expect(ensureBinary("1.2.6")).resolves.toBe(expectedPath);
      expect(binaryFetches).toBe(1);
      expect(probes).toEqual([]);
    } finally {
      __setOffThreadVersionProbeForTests(null);
    }
  });

  test("sweeps stale partial download artifacts after acquiring the lock", async () => {
    const { downloadBinary, getBinaryName } = await import(
      `../downloader.js?stale-temp-sweep-${Date.now()}`
    );
    const assetName = currentAssetName();
    const payload = Buffer.from("partial artifact sweep");
    const sha256 = createHash("sha256").update(payload).digest("hex");
    const versionedDir = join(tmpDir, "aft", "bin", "v1.2.4");
    const staleTemp = join(versionedDir, `${getBinaryName()}.interrupted.tmp`);
    const freshTemp = join(versionedDir, `${getBinaryName()}.active.tmp`);
    mkdirSync(versionedDir, { recursive: true });
    writeFileSync(staleTemp, "interrupted");
    writeFileSync(freshTemp, "active");
    const staleTime = new Date(Date.now() - 11 * 60_000);
    utimesSync(staleTemp, staleTime, staleTime);

    globalThis.fetch = (async (url: string | URL | Request) => {
      if (String(url).endsWith("checksums.sha256")) {
        return new Response(`${sha256}  ${assetName}\n`, { status: 200 });
      }
      return new Response(payload, { status: 200 });
    }) as typeof fetch;

    await expect(downloadBinary("v1.2.4")).resolves.toBe(join(versionedDir, getBinaryName()));
    expect(existsSync(staleTemp)).toBe(false);
    expect(existsSync(freshTemp)).toBe(true);
  });

  test("rejects a checksum-mismatched partial download before promotion", async () => {
    const { downloadBinary, getBinaryName } = await import(
      `../downloader.js?checksum-promotion-${Date.now()}`
    );
    const assetName = currentAssetName();
    const payload = Buffer.from("untrusted partial artifact");
    const mismatchedHash = createHash("sha256").update("different bytes").digest("hex");
    const versionedDir = join(tmpDir, "aft", "bin", "v1.2.5");

    globalThis.fetch = (async (url: string | URL | Request) => {
      if (String(url).endsWith("checksums.sha256")) {
        return new Response(`${mismatchedHash}  ${assetName}\n`, { status: 200 });
      }
      return new Response(payload, { status: 200 });
    }) as typeof fetch;

    await expect(downloadBinary("v1.2.5")).resolves.toBeNull();
    expect(existsSync(join(versionedDir, getBinaryName()))).toBe(false);
    expect(readdirSync(versionedDir).filter((name) => name.endsWith(".tmp"))).toEqual([]);
  });

  test("ensureBinary redownloads mismatched versioned cache entries", async () => {
    if (!shellFixtureAvailable()) return;

    const { ensureBinary, getBinaryName, readBinaryVersion } = await import(
      `../downloader.js?ensure-cache-validate-${Date.now()}`
    );
    const assetName = currentAssetName();
    const payload = Buffer.from("#!/bin/sh\necho aft 1.2.3\n");
    const sha256 = createHash("sha256").update(payload).digest("hex");
    const versionedDir = join(tmpDir, "aft", "bin", "v1.2.3");
    const cachedPath = join(versionedDir, getBinaryName());
    let binaryFetches = 0;

    mkdirSync(versionedDir, { recursive: true });
    writeFileSync(cachedPath, '#!/bin/sh\necho "aft 9.9.9"\n');
    chmodSync(cachedPath, 0o755);
    expect(readBinaryVersion(cachedPath)).toBe("9.9.9");

    globalThis.fetch = (async (url: string | URL | Request) => {
      const rawUrl = String(url);
      if (rawUrl.endsWith("checksums.sha256")) {
        return new Response(`${sha256}  ${assetName}\n`, { status: 200 });
      }
      binaryFetches += 1;
      return new Response(payload, {
        status: 200,
        headers: { "content-length": String(payload.byteLength) },
      });
    }) as typeof fetch;

    await expect(ensureBinary("v1.2.3")).resolves.toBe(cachedPath);
    expect(binaryFetches).toBe(1);
    expect(readFileSync(cachedPath, "utf8")).toContain("1.2.3");
  });

  test("download lock release preserves a reclaimed lock owned by another process", async () => {
    const { __test__ } = await import(`../downloader.js?download-lock-${Date.now()}`);
    const lockDir = join(tmpDir, "lock-owner");
    const lockPath = join(lockDir, ".download.lock");
    mkdirSync(lockDir, { recursive: true });

    const release = await __test__.acquireDownloadLock(lockPath);
    writeFileSync(lockPath, "other-owner");
    release();

    expect(readFileSync(lockPath, "utf8")).toBe("other-owner");
  });

  test("reclaims a lock owned by a dead local PID immediately", async () => {
    const { __test__ } = await import(`../downloader.js?download-lock-dead-pid-${Date.now()}`);
    const lockDir = join(tmpDir, "dead-owner");
    const lockPath = join(lockDir, ".download.lock");
    const deadPid = 999_999_999;
    expect(() => process.kill(deadPid, 0)).toThrow();
    mkdirSync(lockDir, { recursive: true });
    writeFileSync(lockPath, `${deadPid}:${Date.now()}:interrupted`);

    const release = await __test__.acquireDownloadLock(lockPath, {
      timeoutMs: 50,
      staleMs: 1_000,
      pollIntervalMs: 5,
    });

    expect(JSON.parse(readFileSync(lockPath, "utf8"))).toMatchObject({
      pid: process.pid,
      hostname: hostname(),
    });
    release();
    expect(existsSync(lockPath)).toBe(false);
  });

  test("respects a fresh lock held by a live local writer", async () => {
    const { __test__ } = await import(`../downloader.js?download-lock-live-pid-${Date.now()}`);
    const lockDir = join(tmpDir, "live-owner");
    const lockPath = join(lockDir, ".download.lock");
    mkdirSync(lockDir, { recursive: true });

    const release = await __test__.acquireDownloadLock(lockPath);
    await expect(
      __test__.acquireDownloadLock(lockPath, {
        timeoutMs: 30,
        staleMs: 1_000,
        pollIntervalMs: 5,
      }),
    ).rejects.toThrow("Timed out waiting for download lock");
    expect(existsSync(lockPath)).toBe(true);
    release();
  });

  test("reclaims an old lock whose live-looking PID belongs to another host", async () => {
    const { __test__ } = await import(`../downloader.js?download-lock-foreign-${Date.now()}`);
    const lockDir = join(tmpDir, "foreign-owner");
    const lockPath = join(lockDir, ".download.lock");
    mkdirSync(lockDir, { recursive: true });
    const foreignHostname = hostname() === "other-host" ? "other-host-2" : "other-host";
    writeFileSync(
      lockPath,
      JSON.stringify({ pid: process.pid, hostname: foreignHostname, createdAt: Date.now() }),
    );
    const staleTime = new Date(Date.now() - 1_000);
    utimesSync(lockPath, staleTime, staleTime);

    const release = await __test__.acquireDownloadLock(lockPath, {
      timeoutMs: 50,
      staleMs: 20,
      pollIntervalMs: 5,
    });

    expect(JSON.parse(readFileSync(lockPath, "utf8"))).toMatchObject({
      pid: process.pid,
      hostname: hostname(),
    });
    release();
    expect(existsSync(lockPath)).toBe(false);
  });

  test("rejects oversized advertised downloads before buffering", async () => {
    const { downloadBinary, getBinaryName } = await import(
      `../downloader.js?transport-oversize-${Date.now()}`
    );
    const assetName = currentAssetName();
    const payload = Buffer.from("small");
    const sha256 = createHash("sha256").update(payload).digest("hex");

    globalThis.fetch = (async (url: string | URL | Request) => {
      const rawUrl = String(url);
      if (rawUrl.endsWith("checksums.sha256")) {
        return new Response(`${sha256}  ${assetName}\n`, { status: 200 });
      }
      return new Response(payload, {
        status: 200,
        headers: { "content-length": String(201 * 1024 * 1024) },
      });
    }) as typeof fetch;

    await expect(downloadBinary("v1.2.4")).resolves.toBeNull();
    expect(existsSync(join(tmpDir, "aft", "bin", "v1.2.4", getBinaryName()))).toBe(false);
  });
});
