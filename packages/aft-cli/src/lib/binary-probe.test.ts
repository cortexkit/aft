/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { chmodSync, mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, join } from "node:path";
import { withEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import {
  findAftBinary,
  normalizeBinaryVersion,
  probeAftBinary,
  probeBinaryVersion,
} from "./binary-probe.js";
import { getAftBinaryName } from "./paths.js";

function writeFakeAft(path: string, body: string): void {
  writeFileSync(path, `#!/bin/sh\n${body}\n`);
  chmodSync(path, 0o755);
}

interface IsolatedInstall {
  /** Env pointing the cache and PATH lookups at empty temporary directories. */
  env: Record<string, string>;
  /** Path of the cached binary for a `v<semver>` directory name. */
  cached(versionDir: string): string;
  root: string;
  cacheBinDir: string;
}

/**
 * Build the situation from the bug report: binaries only in the versioned
 * cache, with no `aft` on PATH and no platform package.
 *
 * The `~/.cargo/bin` lookup cannot be redirected from inside a running
 * process, so these tests assert on resolution order rather than on a globally
 * empty machine; the cache is searched first, so a cargo install on the
 * developer's machine cannot make them pass.
 */
function isolatedInstall(prefix: string, versionDirs: string[]): IsolatedInstall {
  const root = mkdtempSync(join(tmpdir(), prefix));
  const cacheRoot = join(root, "cache");
  const cacheBinDir = join(cacheRoot, "bin");
  const emptyPathDir = join(root, "empty-path");
  mkdirSync(cacheBinDir, { recursive: true });
  mkdirSync(emptyPathDir, { recursive: true });

  const cached = (versionDir: string) => join(cacheBinDir, versionDir, getAftBinaryName());
  for (const versionDir of versionDirs) {
    mkdirSync(join(cacheBinDir, versionDir), { recursive: true });
    // The cache candidate path is constructed by us (never the CLI shim), so it
    // is resolved without the native-executable guard; a shell script stands in.
    writeFakeAft(cached(versionDir), `printf "aft ${versionDir.replace(/^v/, "")}\\n"`);
  }

  return {
    env: { AFT_CACHE_DIR: cacheRoot, PATH: emptyPathDir },
    cached,
    root,
    cacheBinDir,
  };
}

describe("binary probe version validation", () => {
  test("accepts only semver-shaped aft version output", () => {
    expect(normalizeBinaryVersion("aft 1.2.3\n")).toBe("1.2.3");
    expect(normalizeBinaryVersion("1.2.3-beta.1\n")).toBe("1.2.3-beta.1");
    expect(normalizeBinaryVersion("not-aft 1.2.3\n")).toBeNull();
    expect(normalizeBinaryVersion("hello from another binary\n")).toBeNull();
  });

  test("reports a version-mismatched cache candidate as unmatched", async () => {
    const root = mkdtempSync(join(tmpdir(), "aft-cli-binary-probe-audit-"));
    const cacheDir = join(root, "cache", "bin", "v9.8.7");
    mkdirSync(cacheDir, { recursive: true });
    // The cache candidate path is constructed by us (never the CLI shim), so it
    // is probed without the native-executable guard; a fake shell binary is a
    // valid stand-in here.
    writeFakeAft(join(cacheDir, getAftBinaryName()), 'printf "aft 8.0.0\\n"');

    await withEnv(
      {
        AFT_CACHE_DIR: join(root, "cache"),
        // No native PATH binary available, so resolution must not match.
        PATH: process.env.PATH ?? "",
      },
      () => {
        const probe = probeAftBinary("9.8.7");
        expect(probe.version).toBeNull();
        expect(probe.candidates).toContainEqual(
          expect.objectContaining({ status: "unmatched", version: "8.0.0" }),
        );
      },
    );
  });

  test("reports a non-semver cache candidate as invalid instead of healthy", async () => {
    const root = mkdtempSync(join(tmpdir(), "aft-cli-binary-probe-invalid-"));
    const cacheDir = join(root, "cache", "bin", "v7.7.7");
    mkdirSync(cacheDir, { recursive: true });
    writeFakeAft(join(cacheDir, getAftBinaryName()), 'printf "definitely not aft\\n"');

    await withEnv(
      {
        AFT_CACHE_DIR: join(root, "cache"),
        PATH: process.env.PATH ?? "",
      },
      () => {
        expect(probeBinaryVersion("7.7.7")).toBeNull();
        const probe = probeAftBinary("7.7.7");
        expect(probe.candidates).toContainEqual(expect.objectContaining({ status: "invalid" }));
      },
    );
  });

  test("skips a script-shim `aft` on PATH (fork-bomb guard)", async () => {
    // A `which aft` hit that is a node/sh script shim (e.g. the CLI's own npx
    // bin) must never be probed — probing it re-enters the CLI and fork-bombs.
    // Even though this shim prints a perfectly valid version, it must be
    // filtered out before any --version invocation.
    const root = mkdtempSync(join(tmpdir(), "aft-cli-binary-probe-shim-"));
    const pathDir = join(root, "path");
    mkdirSync(pathDir, { recursive: true });
    writeFakeAft(join(pathDir, getAftBinaryName()), 'printf "aft 7.7.7\\n"');

    await withEnv(
      {
        AFT_CACHE_DIR: join(root, "cache"),
        PATH: `${pathDir}${delimiter}${process.env.PATH ?? ""}`,
      },
      () => {
        const probe = probeAftBinary("7.7.7");
        // The shim is native-filtered, so it never becomes a candidate at all —
        // no "matched" 7.7.7 from it, and resolution finds nothing.
        expect(probe.version).toBeNull();
        expect(
          probe.candidates.some((c) => c.path.startsWith(pathDir) && c.status === "matched"),
        ).toBe(false);
      },
    );
  });
});

describe("binary resolution without a preferred version", () => {
  test("resolves a binary installed only in the versioned cache", async () => {
    // The reported bug: `aft doctor --fix` downloads into <cache>/v<version>/,
    // and the next command claimed no binary existed because it only searched
    // the cache when a caller named a version.
    const install = isolatedInstall("aft-cli-cache-only-", ["v0.56.2"]);

    await withEnv(install.env, () => {
      expect(findAftBinary()).toBe(install.cached("v0.56.2"));
    });
  });

  test("picks the newest cached version by semver, not lexically", async () => {
    // v0.9.0 sorts after v0.10.0 as text, and v0.0.0 exists on real machines;
    // either winning would hand the caller a stale or empty install.
    const install = isolatedInstall("aft-cli-cache-newest-", ["v0.0.0", "v0.9.0", "v0.10.0"]);

    await withEnv(install.env, () => {
      expect(findAftBinary()).toBe(install.cached("v0.10.0"));
    });
  });

  test("ignores cache entries that are not v<semver> directories", async () => {
    const install = isolatedInstall("aft-cli-cache-junk-", ["v0.9.0"]);
    mkdirSync(join(install.cacheBinDir, "tmp-download"), { recursive: true });
    writeFakeAft(join(install.cacheBinDir, "tmp-download", getAftBinaryName()), "exit 0");

    await withEnv(install.env, () => {
      expect(findAftBinary()).toBe(install.cached("v0.9.0"));
    });
  });

  test("skips a script-shim `aft` on PATH when falling back to the cache", async () => {
    // The fork-bomb guard must still hold on the code path that now also
    // searches the cache: a shebang shim first on PATH is never returned, even
    // when the cache is empty and the shim is the only `aft` in sight. The real
    // PATH is kept so `which` itself is runnable and actually finds the shim.
    const install = isolatedInstall("aft-cli-cache-shim-", []);
    const pathDir = join(install.root, "path-with-shim");
    const shim = join(pathDir, getAftBinaryName());
    mkdirSync(pathDir, { recursive: true });
    writeFakeAft(shim, 'printf "aft 7.7.7\\n"');

    await withEnv(
      { ...install.env, PATH: `${pathDir}${delimiter}${process.env.PATH ?? ""}` },
      () => {
        expect(findAftBinary()).not.toBe(shim);
      },
    );
  });
});

describe("preferred version resolution", () => {
  test("an explicitly preferred version beats the newest cached version", async () => {
    const install = isolatedInstall("aft-cli-cache-preferred-", ["v0.9.0", "v0.10.0"]);

    await withEnv(install.env, () => {
      expect(findAftBinary("0.9.0")).toBe(install.cached("v0.9.0"));
      expect(findAftBinary("v0.9.0")).toBe(install.cached("v0.9.0"));
    });
  });
});
