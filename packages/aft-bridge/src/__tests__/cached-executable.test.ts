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
    pruneExecutableCache(root, "another identity", Date.now(), (source, destination) => {
      const refreshed = new Date();
      utimesSync(source, refreshed, refreshed);
      renameSync(source, destination);
    });
    expect(readFileSync(image).toString()).toBe("retained");
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
