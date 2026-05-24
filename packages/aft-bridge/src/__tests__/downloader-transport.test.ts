/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { existsSync, mkdtempSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { PLATFORM_ARCH_MAP, PLATFORM_ASSET_MAP } from "../platform.js";
import { acquireEnv } from "./test-utils/env-guard.js";

describe("downloadBinary hardened transport", () => {
  let tmpDir: string;
  let releaseEnv: (() => void) | undefined;
  let originalFetch: typeof fetch;

  beforeEach(async () => {
    tmpDir = mkdtempSync(join(tmpdir(), "aft-download-test-"));
    releaseEnv = await acquireEnv({ XDG_CACHE_HOME: tmpDir });
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
