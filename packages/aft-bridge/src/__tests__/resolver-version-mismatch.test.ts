/// <reference path="../bun-test.d.ts" />

/**
 * Resolver version-mismatch test — verifies that `findBinarySync` rejects
 * an npm platform binary whose `--version` does not match the requested
 * `expectedVersion`, and falls through to PATH lookup instead.
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
import { existsSync, mkdirSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { findBinarySync, readBinaryVersion, __test__ as resolverTest } from "../resolver.js";
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

  test("returns exact-version cached binary after probing --version", () => {
    const binaryPath = writeCachedVersion("v1.2.3", "1.2.3");

    // Precondition: the cached binary actually exists at the expected path.
    expect(existsSync(binaryPath)).toBe(true);

    // Precondition: the fake binary actually reports the expected version.
    expect(readBinaryVersion(binaryPath)).toBe("1.2.3");

    expect(findBinarySync("1.2.3")).toBe(binaryPath);
  });

  test("logs the successful resolution path and source", () => {
    const binaryPath = writeCachedVersion("v1.2.3", "1.2.3");
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

  test("skips mislabeled newer cached binary instead of accepting directory name", () => {
    const binaryPath = writeCachedVersion("v1.2.3", "9.9.9");

    expect(existsSync(binaryPath)).toBe(true);

    expect(findBinarySync("1.2.3")).toBeNull();
  });
});

describe("findBinarySync PATH lookup parsing", () => {
  test("splits CRLF-separated Windows where output into individual candidates", () => {
    expect(
      resolverTest.parsePathLookupOutput("C:\\tools\\aft.exe\r\nC:\\other\\aft.exe\r\n"),
    ).toEqual(["C:\\tools\\aft.exe", "C:\\other\\aft.exe"]);
  });
});

describe.skipIf(skipPosixPathLookup)("findBinarySync PATH/cargo validation", () => {
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
    releaseEnv?.();
    releaseEnv = undefined;
    rmSync(tmpDir, { recursive: true, force: true });
  });

  function writeFakeAft(path: string, reportedVersion: string): void {
    writeAftVersionFixture(path, reportedVersion);
  }

  test("skips mismatched PATH candidate and falls through to matching cargo binary", () => {
    const pathBinary = join(tmpDir, "path-bin", "aft");
    const cargoBinary = join(tmpDir, "home", ".cargo", "bin", "aft");
    writeFakeAft(pathBinary, "9.9.9");
    writeFakeAft(cargoBinary, "1.2.3");

    expect(readBinaryVersion(pathBinary)).toBe("9.9.9");
    expect(readBinaryVersion(cargoBinary)).toBe("1.2.3");
    expect(findBinarySync("1.2.3")).toBe(cargoBinary);
  });
});

describe("findBinarySync AFT_BINARY_PATH override", () => {
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

  test("uses the explicit hermetic binary before caches and PATH", async () => {
    const binaryPath = writeAftVersionFixture(join(tmpDir, "aft-explicit"), "1.2.3");
    releaseEnv = await acquireEnv({
      AFT_BINARY_PATH: binaryPath,
      AFT_CACHE_DIR: undefined,
      XDG_CACHE_HOME: tmpDir,
      HOME: tmpDir,
      PATH: "",
    });

    expect(findBinarySync("1.2.3")).toBe(binaryPath);
  });

  test("fails closed instead of touching operator resolution sources", async () => {
    const missing = join(tmpDir, "missing-aft");
    releaseEnv = await acquireEnv({
      AFT_BINARY_PATH: missing,
      AFT_CACHE_DIR: undefined,
      XDG_CACHE_HOME: tmpDir,
      HOME: tmpDir,
      PATH: "",
    });

    expect(() => findBinarySync("1.2.3")).toThrow(/AFT_BINARY_PATH.*native executable/);
  });
});
