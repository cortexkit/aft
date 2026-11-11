import { expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  renameSync,
  rmSync,
  statSync,
  symlinkSync,
  utimesSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import {
  cachedExecutableBytes,
  linkCachedExecutable,
  pruneExecutableCache,
} from "./test-utils/cached-executable.js";

test("binary executable bytes reuse one immutable content-addressed path", () => {
  const bytes = Buffer.from(`\u0000AFT test executable ${randomUUID()}\n`);
  const first = cachedExecutableBytes(bytes);
  const before = statSync(first);
  const second = cachedExecutableBytes(bytes);
  const after = statSync(second);

  expect(second).toBe(first);
  expect(readFileSync(second)).toEqual(bytes);
  expect(after.mtimeMs).toBe(before.mtimeMs);
  expect(after.ino).toBe(before.ino);
  expect(second.split(/[\\/]/).at(-2)).toMatch(/^[a-f0-9]{64}$/);
});

test("executable cache prunes only stale siblings and refreshes every hit", () => {
  const root = mkdtempSync(join(tmpdir(), "aft-cache-retention-"));
  try {
    const current = cachedExecutableBytes(Buffer.from("current"), root);
    const now = 1_700_000_000_000;
    const retention = 24 * 60 * 60 * 1000;
    const identity = (digit: string, age: number) => {
      const directory = join(root, digit.repeat(64));
      mkdirSync(directory);
      const modified = new Date(now - age);
      utimesSync(directory, modified, modified);
      return directory;
    };
    const stale = identity("a", retention + 1000);
    const fresh = identity("b", retention - 1000);
    const boundary = identity("c", retention);
    const future = identity("d", -1000);
    const old = new Date(now - retention - 1000);
    utimesSync(dirname(current), old, old);
    pruneExecutableCache(root, dirname(current), now);
    expect(existsSync(stale)).toBe(false);
    for (const retained of [fresh, boundary, future, dirname(current)])
      expect(existsSync(retained)).toBe(true);
    expect(cachedExecutableBytes(Buffer.from("current"), root)).toBe(current);
    expect(statSync(dirname(current)).mtimeMs).toBeGreaterThan(now);
    cachedExecutableBytes(Buffer.from("next image"), root);
    expect(existsSync(current)).toBe(true);
    expect(existsSync(fresh)).toBe(false);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("pruning restores an identity refreshed between scan and claim", () => {
  const root = mkdtempSync(join(tmpdir(), "aft-cache-refresh-"));
  try {
    const image = cachedExecutableBytes(Buffer.from("retained"), root);
    const directory = dirname(image);
    utimesSync(directory, new Date(0), new Date(0));
    let claims = 0;
    pruneExecutableCache(
      root,
      "another identity",
      Date.now(),
      (source, destination) => {
        claims++;
        const refreshed = new Date();
        utimesSync(source, refreshed, refreshed);
        renameSync(source, destination);
      },
      undefined,
      { usage: { pidAlive: () => false, inUse: () => false }, freeBytes: () => 40 * 1024 ** 3 },
    );
    expect(claims).toBe(1);
    expect(readFileSync(image).toString()).toBe("retained");
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("executable cache body errors take precedence over unexpected cleanup errors", () => {
  const root = mkdtempSync(join(tmpdir(), "aft-cache-errors-"));
  try {
    const image = cachedExecutableBytes(Buffer.from("stale"), root);
    utimesSync(dirname(image), new Date(0), new Date(0));
    const bodyError = new Error("claim failed");
    const cleanupError = new Error("cleanup failed");
    let cleanups = 0;
    const probes = {
      usage: { pidAlive: () => false, inUse: () => false },
      freeBytes: () => 40 * 1024 ** 3,
    };
    expect(() =>
      pruneExecutableCache(
        root,
        "another identity",
        Date.now(),
        () => {
          throw bodyError;
        },
        () => {
          cleanups++;
          throw cleanupError;
        },
        probes,
      ),
    ).toThrow(bodyError);
    expect(cleanups).toBe(1);
    expect(() =>
      pruneExecutableCache(
        root,
        "another identity",
        Date.now(),
        renameSync,
        () => {
          throw cleanupError;
        },
        probes,
      ),
    ).toThrow(cleanupError);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("executable cache count cap keeps live leases and ignores expired reused PIDs", () => {
  const root = mkdtempSync(join(tmpdir(), "aft-cache-count-"));
  try {
    const now = 1_700_000_000_000;
    const directory = (number: number, age: number) => {
      const path = join(root, number.toString(16).padStart(64, "0"));
      mkdirSync(path);
      writeFileSync(join(path, "image"), "fixture");
      utimesSync(path, new Date(now - age), new Date(now - age));
      return path;
    };
    const lease = (path: string, age: number) => {
      const modified = statSync(path).mtime;
      mkdirSync(join(path, ".leases"));
      const marker = join(path, ".leases", "123");
      writeFileSync(marker, "");
      utimesSync(marker, new Date(now - age), new Date(now - age));
      utimesSync(path, modified, modified);
    };
    const unused = Array.from({ length: 6 }, (_, index) =>
      directory(index + 1, (index + 1) * 1000),
    );
    const active = directory(20, 4 * 3600_000);
    lease(active, 0);
    const expired = directory(21, 4 * 3600_000);
    lease(expired, 3 * 3600_000);
    const opened = directory(22, 25 * 3600_000);
    pruneExecutableCache(root, "current", now, renameSync, undefined, {
      usage: { pidAlive: (pid) => pid === 123, inUse: (path) => path === opened },
      freeBytes: () => 40 * 1024 ** 3,
    });
    for (const [index, path] of unused.entries()) expect(existsSync(path)).toBe(index < 4);
    expect(existsSync(active)).toBe(true);
    expect(existsSync(expired)).toBe(false);
    expect(existsSync(opened)).toBe(true);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("executable cache low disk evicts unused images only after thirty minutes", () => {
  const root = mkdtempSync(join(tmpdir(), "aft-cache-pressure-"));
  try {
    const now = 1_700_000_000_000;
    const paths = [1801, 1800, 1799].map((age, index) => {
      const path = join(root, String(index).repeat(64));
      mkdirSync(path);
      utimesSync(path, new Date(now - age * 1000), new Date(now - age * 1000));
      return path;
    });
    const usage = { pidAlive: () => false, inUse: () => false };
    pruneExecutableCache(root, "current", now, renameSync, undefined, {
      usage,
      freeBytes: () => 40 * 1024 ** 3,
    });
    expect(paths.every(existsSync)).toBe(true);
    pruneExecutableCache(root, "current", now, renameSync, undefined, {
      usage,
      freeBytes: () => 40 * 1024 ** 3 - 1,
    });
    expect(existsSync(paths[0]!)).toBe(false);
    expect(existsSync(paths[1]!)).toBe(true);
    expect(existsSync(paths[2]!)).toBe(true);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("executable cache republishes an entry deleted between lookup and linking", () => {
  const root = mkdtempSync(join(tmpdir(), "aft-cache-retry-"));
  try {
    let links = 0;
    const source = "#!/bin/sh\nexit 0\n";
    const path = linkCachedExecutable(
      join(root, "fixture"),
      source,
      join(root, "cache"),
      (cached, destination) => {
        if (++links === 1) rmSync(dirname(cached), { recursive: true });
        symlinkSync(cached, destination);
      },
    );
    expect(links).toBe(2);
    expect(readFileSync(path).toString()).toBe(source);
    if (process.platform !== "win32") expect(spawnSync(path).status).toBe(0);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
