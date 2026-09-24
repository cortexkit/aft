/// <reference path="../bun-test.d.ts" />

/**
 * Resolver version-mismatch test — verifies that the resolver never accepts a
 * binary whose version does not match the requested `expectedVersion`, and
 * that the synchronous resolver establishes identity without running anything.
 *
 * Regression case (caught during v0.23 Pi RPC e2e dogfooding): a workspace
 * upgraded to plugin v0.22.x can still have a bun-hoisted older
 * `@cortexkit/aft-<platform>` symlink in node_modules (e.g. v0.19.5). The
 * resolver would happily run that older binary, producing stale behavior
 * (in the original repro: `bgb-` task slugs instead of `bash-`).
 *
 * No module mocking — uses a real fake binary directory and writes a small
 * executable fixture that emits a controlled `--version` output. The npm-package
 * resolution leg cannot be exercised without `node_modules/@cortexkit/aft-*`
 * present, so this test focuses on the version-check helper directly.
 */
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  chmodSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  utimesSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, dirname, join } from "node:path";
import {
  __waitForIdentityWritesForTests,
  identitySidecarPath,
  readBinaryIdentity,
  writeBinaryIdentitySidecar,
} from "../binary-identity.js";
import {
  __setEnsureBinaryForTests,
  __setNpmPlatformPackageForTests,
  __waitForCacheCopiesForTests,
  findBinary,
  findBinarySync,
  readBinaryVersion,
  __test__ as resolverTest,
} from "../resolver.js";
import { writeAftFixture, writeAftVersionFixture } from "./test-utils/aft-executable-fixture.js";
import { acquireEnv } from "./test-utils/env-guard.js";

// PATH/cargo resolution below hard-codes POSIX path layout and `aft` (without
// `.exe`) fixture names. The direct version/cache tests use native executable
// fixtures on POSIX so they do not depend on shebang shell-script dispatch.
const skipPosixPathLookup = process.platform === "win32";

describe("readBinaryVersion", () => {
  let tmpDir: string;

  beforeEach(() => {
    tmpDir = mkdtempSync(join(tmpdir(), "aft-version-test-"));
  });

  afterEach(() => {
    rmSync(tmpDir, { recursive: true, force: true });
  });

  test("parses 'aft 0.22.1' style output", () => {
    const fakeBin = writeAftVersionFixture(join(tmpDir, "fake-aft"), "0.22.1");
    expect(readBinaryVersion(fakeBin)).toBe("0.22.1");
  });

  test("parses 'aft 0.19.5' (older pre-rename version)", () => {
    const fakeBin = writeAftVersionFixture(join(tmpDir, "fake-aft"), "0.19.5");
    expect(readBinaryVersion(fakeBin)).toBe("0.19.5");
  });

  test("returns null for empty output", () => {
    const fakeBin = writeAftFixture(join(tmpDir, "fake-aft"), { exitCode: 0 });
    expect(readBinaryVersion(fakeBin)).toBeNull();
  });

  test("parses stderr-only version output when stdout is empty", () => {
    const fakeBin = writeAftFixture(join(tmpDir, "fake-aft"), { stderr: "aft 0.74.0\n" });
    expect(readBinaryVersion(fakeBin)).toBe("0.74.0");
  });

  test("returns null for binaries that fail", () => {
    const fakeBin = writeAftFixture(join(tmpDir, "fake-aft"), { exitCode: 1 });
    // Non-zero exit with no stdout is null
    expect(readBinaryVersion(fakeBin)).toBeNull();
  });

  test("returns null when path does not exist", () => {
    expect(readBinaryVersion(join(tmpDir, "does-not-exist"))).toBeNull();
  });

  test("strips 'v' prefix not applied — readBinaryVersion returns bare version", () => {
    // The cache layout uses `v<version>` paths but readBinaryVersion returns
    // the bare version without the `v` prefix. Callers (e.g.
    // findBinarySync's version-mismatch check) compare bare versions, so this
    // is the load-bearing contract: pluginVersion="0.22.1" must equal
    // readBinaryVersion(npm-binary) when no leading "v" is involved.
    const fakeBin = writeAftVersionFixture(join(tmpDir, "fake-aft"), "0.22.1");
    expect(readBinaryVersion(fakeBin)).toBe("0.22.1"); // not "v0.22.1"
  });
});

describe("findBinarySync versioned cache validation", () => {
  let tmpDir: string;
  let releaseEnv: (() => void) | undefined;

  beforeEach(async () => {
    tmpDir = mkdtempSync(join(tmpdir(), "aft-cache-version-test-"));
    // Bun runs test files concurrently in one process. Keep resolver env
    // overrides guarded for the full test so other files cannot clobber them.
    releaseEnv = await acquireEnv({
      AFT_BINARY_PATH: undefined,
      // CI exports an ambient AFT_CACHE_DIR (highest precedence in the shared
      // cache resolver); clear it so the XDG sandbox below actually applies.
      AFT_CACHE_DIR: undefined,
      XDG_CACHE_HOME: tmpDir,
      PATH: "",
      HOME: tmpDir,
    });
  });

  afterEach(() => {
    __setEnsureBinaryForTests(null);
    releaseEnv?.();
    releaseEnv = undefined;
    rmSync(tmpDir, { recursive: true, force: true });
  });

  function writeCachedVersion(dirVersion: string, reportedVersion: string): string {
    const binaryPath = join(
      tmpDir,
      "aft",
      "bin",
      dirVersion,
      process.platform === "win32" ? "aft.exe" : "aft",
    );
    return writeAftVersionFixture(binaryPath, reportedVersion);
  }

  test("returns a cached binary vouched for by its identity sidecar", () => {
    const binaryPath = writeCachedVersion("v1.2.3", "1.2.3");
    writeBinaryIdentitySidecar(binaryPath, "1.2.3", "0".repeat(64));

    expect(findBinarySync("1.2.3")).toBe(binaryPath);
  });

  test("does not trust a cached binary without a sidecar", () => {
    const binaryPath = writeCachedVersion("v1.2.3", "1.2.3");
    expect(existsSync(binaryPath)).toBe(true);

    expect(findBinarySync("1.2.3")).toBeNull();
  });

  test("does not trust a sidecar that records a different version", () => {
    const binaryPath = writeCachedVersion("v1.2.3", "1.2.3");
    writeBinaryIdentitySidecar(binaryPath, "9.9.9", "0".repeat(64));

    expect(findBinarySync("1.2.3")).toBeNull();
  });

  test("logs the successful resolution path and source", () => {
    const binaryPath = writeCachedVersion("v1.2.3", "1.2.3");
    writeBinaryIdentitySidecar(binaryPath, "1.2.3", "0".repeat(64));
    const packageRoot = join(import.meta.dir, "..", "..");
    const result = spawnSync(
      process.execPath,
      [
        "-e",
        'import { findBinarySync } from "./src/resolver.ts"; console.log(findBinarySync("1.2.3"));',
      ],
      {
        cwd: packageRoot,
        env: {
          ...process.env,
          AFT_CACHE_DIR: join(tmpDir, "aft"),
          HOME: tmpDir,
          PATH: "",
        },
        encoding: "utf8",
      },
    );

    expect(result.error).toBeUndefined();
    expect(result.status).toBe(0);
    expect(result.stdout.trim()).toBe(binaryPath);
    expect(result.stderr).toContain(
      `[aft-bridge] Resolved binary from versioned cache: ${binaryPath}`,
    );
  });

  test("findBinary verifies a sidecar-less entry off-thread and records its identity", async () => {
    const binaryPath = writeCachedVersion("v1.2.3", "1.2.3");
    __setEnsureBinaryForTests(async () => {
      throw new Error("a verified cache entry must not trigger a download");
    });

    await expect(findBinary("1.2.3")).resolves.toBe(binaryPath);
    await __waitForIdentityWritesForTests();

    const identity = readBinaryIdentity(binaryPath);
    expect(identity?.version).toBe("1.2.3");
    expect(identity?.sha256).toBe(
      createHash("sha256").update(readFileSync(binaryPath)).digest("hex"),
    );
    // The next lookup is stat-only.
    expect(findBinarySync("1.2.3")).toBe(binaryPath);
  });

  test("skips mislabeled newer cached binary instead of accepting directory name", async () => {
    const binaryPath = writeCachedVersion("v1.2.3", "9.9.9");
    expect(existsSync(binaryPath)).toBe(true);
    const downloads: Array<string | undefined> = [];
    __setEnsureBinaryForTests(async (version) => {
      downloads.push(version);
      return "/downloaded/aft";
    });

    expect(findBinarySync("1.2.3")).toBeNull();
    await expect(findBinary("1.2.3")).resolves.toBe("/downloaded/aft");
    expect(downloads).toEqual(["1.2.3"]);
    expect(existsSync(identitySidecarPath(binaryPath))).toBe(false);
  });

  test("a binary replaced under its sidecar is re-verified and a wrong version is not used", async () => {
    const binaryPath = writeCachedVersion("v1.2.3", "1.2.3");
    writeBinaryIdentitySidecar(binaryPath, "1.2.3", "0".repeat(64));
    // Another writer swaps different bytes in: the sidecar no longer matches.
    // The fixtures are the same size, and on Linux the recreated file can
    // reuse the inode and land in the same coarse timestamp tick, which would
    // make the swap invisible to a stat comparison. Real cache writers replace
    // by temp file and rename, so they always change the inode; here the
    // replacement's mtime is moved explicitly so the test exercises the
    // mismatch path instead of depending on filesystem timing.
    rmSync(binaryPath);
    writeCachedVersion("v1.2.3", "9.9.9");
    const later = new Date(Date.now() + 5_000);
    utimesSync(binaryPath, later, later);
    __setEnsureBinaryForTests(async () => "/downloaded/aft");

    expect(findBinarySync("1.2.3")).toBeNull();
    await expect(findBinary("1.2.3")).resolves.toBe("/downloaded/aft");
  });
});

describe("PATH candidate scan", () => {
  test("lists existing aft executables in PATH order and ignores relative entries", () => {
    const root = mkdtempSync(join(tmpdir(), "aft-path-scan-"));
    try {
      const ext = process.platform === "win32" ? ".exe" : "";
      const first = join(root, "first");
      const second = join(root, "second");
      const empty = join(root, "empty");
      mkdirSync(empty, { recursive: true });
      writeAftVersionFixture(join(first, `aft${ext}`), "1.0.0");
      writeAftVersionFixture(join(second, `aft${ext}`), "1.0.0");
      const PATH = ["relative/bin", empty, first, second, first].join(delimiter);

      expect(resolverTest.pathCandidates({ PATH }, ext)).toEqual([
        join(first, `aft${ext}`),
        join(second, `aft${ext}`),
      ]);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });
});

describe.skipIf(skipPosixPathLookup)("findBinary PATH/cargo validation", () => {
  let tmpDir: string;
  let releaseEnv: (() => void) | undefined;

  beforeEach(async () => {
    tmpDir = mkdtempSync(join(tmpdir(), "aft-path-version-test-"));
    const pathDir = join(tmpDir, "path-bin");
    mkdirSync(pathDir, { recursive: true });
    releaseEnv = await acquireEnv({
      AFT_BINARY_PATH: undefined,
      AFT_CACHE_DIR: undefined,
      XDG_CACHE_HOME: join(tmpDir, "cache"),
      PATH: `${pathDir}:${process.env.PATH ?? ""}`,
      HOME: join(tmpDir, "home"),
    });
  });

  afterEach(() => {
    __setEnsureBinaryForTests(null);
    releaseEnv?.();
    releaseEnv = undefined;
    rmSync(tmpDir, { recursive: true, force: true });
  });

  test("skips mismatched PATH candidate and falls through to matching cargo binary", async () => {
    const pathBinary = join(tmpDir, "path-bin", "aft");
    const cargoBinary = join(tmpDir, "home", ".cargo", "bin", "aft");
    writeAftVersionFixture(pathBinary, "9.9.9");
    writeAftVersionFixture(cargoBinary, "1.2.3");
    __setEnsureBinaryForTests(async () => null);

    // Neither is considered synchronously: their versions are unknown until they run.
    expect(findBinarySync("1.2.3")).toBeNull();
    await expect(findBinary("1.2.3")).resolves.toBe(cargoBinary);
  });
});

describe("AFT_BINARY_PATH override", () => {
  let tmpDir: string;
  let releaseEnv: (() => void) | undefined;

  beforeEach(() => {
    tmpDir = mkdtempSync(join(tmpdir(), "aft-explicit-binary-test-"));
  });

  afterEach(() => {
    releaseEnv?.();
    releaseEnv = undefined;
    rmSync(tmpDir, { recursive: true, force: true });
  });

  async function useExplicit(binaryPath: string): Promise<void> {
    releaseEnv = await acquireEnv({
      AFT_BINARY_PATH: binaryPath,
      AFT_CACHE_DIR: undefined,
      XDG_CACHE_HOME: tmpDir,
      HOME: tmpDir,
      PATH: "",
    });
  }

  test("uses the explicit hermetic binary before caches and PATH", async () => {
    const binaryPath = writeAftVersionFixture(join(tmpDir, "aft-explicit"), "1.2.3");
    await useExplicit(binaryPath);

    expect(findBinarySync("1.2.3")).toBe(binaryPath);
    await expect(findBinary("1.2.3")).resolves.toBe(binaryPath);
  });

  test("findBinary rejects an explicit binary that reports another version", async () => {
    const binaryPath = writeAftVersionFixture(join(tmpDir, "aft-explicit"), "9.9.9");
    await useExplicit(binaryPath);

    await expect(findBinary("1.2.3")).rejects.toThrow(/AFT_BINARY_PATH is incompatible/);
  });

  test("fails closed instead of touching operator resolution sources", async () => {
    await useExplicit(join(tmpDir, "missing-aft"));

    expect(() => findBinarySync("1.2.3")).toThrow(/AFT_BINARY_PATH.*native executable/);
    await expect(findBinary("1.2.3")).rejects.toThrow(/AFT_BINARY_PATH.*native executable/);
  });
});

describe.skipIf(skipPosixPathLookup)("npm platform package copy into the versioned cache", () => {
  let tmpDir: string;
  let execLog: string;
  let npmBinary: string;
  let cachedPath: string;
  let releaseEnv: (() => void) | undefined;

  function writeRecordingStub(path: string, label: string, version: string): void {
    mkdirSync(dirname(path), { recursive: true });
    writeFileSync(
      path,
      `#!/bin/sh\nprintf '%s\\n' "${label} $*" >> ${JSON.stringify(execLog)}\necho "aft ${version}"\n`,
    );
    chmodSync(path, 0o755);
  }

  beforeEach(async () => {
    tmpDir = mkdtempSync(join(tmpdir(), "aft-npm-copy-test-"));
    execLog = join(tmpDir, "exec.log");
    releaseEnv = await acquireEnv({
      AFT_BINARY_PATH: undefined,
      AFT_CACHE_DIR: join(tmpDir, "cache"),
      PATH: "",
      HOME: tmpDir,
    });
    npmBinary = join(tmpDir, "node_modules", "@cortexkit", "aft-test", "bin", "aft");
    writeRecordingStub(npmBinary, "npm", "1.2.3");
    cachedPath = join(tmpDir, "cache", "bin", "v1.2.3", "aft");
    __setNpmPlatformPackageForTests(() => ({ binaryPath: npmBinary, version: "1.2.3" }));
  });

  afterEach(async () => {
    await __waitForCacheCopiesForTests();
    await __waitForIdentityWritesForTests();
    __setNpmPlatformPackageForTests(null);
    __setEnsureBinaryForTests(null);
    releaseEnv?.();
    releaseEnv = undefined;
    rmSync(tmpDir, { recursive: true, force: true });
  });

  function execs(): string {
    return existsSync(execLog) ? readFileSync(execLog, "utf8") : "";
  }

  test("findBinarySync returns the npm binary while the copy runs, then the recorded cache copy", async () => {
    expect(findBinarySync("1.2.3")).toBe(npmBinary);
    // The copy has not landed synchronously: nothing was waited on.
    expect(existsSync(identitySidecarPath(cachedPath))).toBe(false);

    await __waitForCacheCopiesForTests();
    await __waitForIdentityWritesForTests();

    expect(readFileSync(cachedPath, "utf8")).toBe(readFileSync(npmBinary, "utf8"));
    expect(findBinarySync("1.2.3")).toBe(cachedPath);
    expect(execs()).toBe("");
  });

  test("a boot racing the sidecar write never runs the unrecorded cache copy", async () => {
    // State another process leaves between renaming its copy into place and
    // writing the sidecar: a cache entry with no sidecar. Its bytes are a stub
    // that records any execution, so running it unverified would show up.
    writeRecordingStub(cachedPath, "unverified-cache", "1.2.3");
    const staleBytes = readFileSync(cachedPath, "utf8");
    __setEnsureBinaryForTests(async () => {
      throw new Error("no download expected");
    });

    const syncPath = findBinarySync("1.2.3");
    const asyncPath = await findBinary("1.2.3");

    expect(syncPath).toBe(npmBinary);
    // findBinary waits for its own copy, so it may return the cache path, but
    // only after replacing the unrecorded bytes with the npm package's.
    expect([npmBinary, cachedPath]).toContain(asyncPath);
    await __waitForCacheCopiesForTests();
    await __waitForIdentityWritesForTests();
    expect(readFileSync(cachedPath, "utf8")).not.toBe(staleBytes);
    expect(findBinarySync("1.2.3")).toBe(cachedPath);
    expect(execs()).not.toContain("unverified-cache");
    expect(execs()).toBe("");
  });
});
