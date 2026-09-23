import { afterEach, describe, expect, test } from "bun:test";
import {
  chmodSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { RotatingLogSink, resolveAftLogPath, resolveAftStorageRoot } from "../durable-log.js";
import { withEnv } from "./test-utils/env-guard.js";

const cleanup: string[] = [];
afterEach(() => {
  for (const path of cleanup.splice(0)) rmSync(path, { force: true, recursive: true });
});

describe("durable plugin logging", () => {
  test("AFT_STORAGE_DIR wins over configured and legacy roots", async () => {
    const storage = join(tmpdir(), "aft-storage-resolution");
    await withEnv(
      {
        AFT_STORAGE_DIR: storage,
        AFT_CACHE_DIR: join(tmpdir(), "aft-cache-resolution"),
        XDG_DATA_HOME: join(tmpdir(), "aft-xdg-resolution"),
      },
      () => {
        expect(resolveAftStorageRoot(join(tmpdir(), "wire-root"))).toBe(storage);
        expect(resolveAftLogPath("aft-plugin.log")).toBe(join(storage, "logs", "aft-plugin.log"));
      },
    );
  });

  test("explicit configured roots preserve their spelling", async () => {
    await withEnv({ AFT_STORAGE_DIR: undefined }, () => {
      const configured = join(tmpdir(), "aft-explicit", "spelled", "..");
      expect(resolveAftStorageRoot(configured)).toBe(configured);
    });
  });

  // The log root follows the one storage ladder every AFT entry point uses
  // (AFT_STORAGE_DIR, then AFT_CACHE_DIR/aft, then the XDG data root), so a
  // bridge and the Rust binary it spawns never log under different roots.
  test("legacy AFT_CACHE_DIR outranks the computed XDG root, as in the Rust ladder", async () => {
    const xdg = join(tmpdir(), "aft-xdg-resolution");
    const legacyCache = join(tmpdir(), "aft-cache-resolution");
    await withEnv(
      {
        AFT_STORAGE_DIR: undefined,
        AFT_CACHE_DIR: legacyCache,
        XDG_DATA_HOME: xdg,
      },
      () => {
        expect(resolveAftStorageRoot()).toBe(join(legacyCache, "aft"));
      },
    );
    await withEnv(
      {
        AFT_STORAGE_DIR: undefined,
        AFT_CACHE_DIR: undefined,
        XDG_DATA_HOME: xdg,
      },
      () => {
        expect(resolveAftStorageRoot()).toBe(join(xdg, "cortexkit", "aft"));
      },
    );
  });

  test("rotates at the byte threshold and replaces the single backup generation", async () => {
    const root = mkdtempSync(join(tmpdir(), "aft-durable-log-"));
    cleanup.push(root);
    const path = join(root, "logs", "aft-plugin.log");
    const sink = new RotatingLogSink(path, { maxBytes: 10 });

    for (const value of ["aaaa\n", "bbbb\n", "cccc\n", "dddd\n", "eeee\n"]) sink.append(value);
    await sink.drain();

    expect(readFileSync(path, "utf8")).toBe("eeee\n");
    expect(readFileSync(`${path}.1`, "utf8")).toBe("cccc\ndddd\n");
    expect(() => readFileSync(`${path}.2`, "utf8")).toThrow();
  });

  const posixOnly = process.platform === "win32" ? test.skip : test;
  const modeOf = (path: string): number => statSync(path).mode & 0o777;

  posixOnly("creates an owner-only log directory and log files", async () => {
    const root = mkdtempSync(join(tmpdir(), "aft-durable-log-"));
    cleanup.push(root);
    const path = join(root, "storage", "logs", "aft-plugin.log");
    const sink = new RotatingLogSink(path, { maxBytes: 10 });

    for (const value of ["aaaa\n", "bbbb\n", "cccc\n"]) sink.append(value);
    await sink.drain();

    expect(modeOf(join(root, "storage", "logs"))).toBe(0o700);
    expect(modeOf(path)).toBe(0o600);
    expect(modeOf(`${path}.1`)).toBe(0o600);
  });

  posixOnly("tightens an existing world-readable log directory and files", async () => {
    const root = mkdtempSync(join(tmpdir(), "aft-durable-log-"));
    cleanup.push(root);
    const dir = join(root, "logs");
    const path = join(dir, "aft-plugin.log");
    mkdirSync(dir, { mode: 0o755 });
    chmodSync(dir, 0o755);
    for (const file of [path, `${path}.1`]) {
      writeFileSync(file, "old\n");
      chmodSync(file, 0o644);
    }

    const sink = new RotatingLogSink(path);
    sink.append("new\n");
    await sink.drain();

    expect(modeOf(dir)).toBe(0o700);
    expect(modeOf(path)).toBe(0o600);
    expect(modeOf(`${path}.1`)).toBe(0o600);
    expect(readFileSync(path, "utf8")).toBe("old\nnew\n");
  });
});
