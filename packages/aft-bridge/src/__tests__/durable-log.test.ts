import { afterEach, describe, expect, spyOn, test } from "bun:test";
import {
  chmodSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  renameSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { open } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { RotatingLogSink, resolveAftLogPath, resolveAftStorageRoot } from "../durable-log.js";
import { resolvePluginLogPath } from "../storage-paths.js";
import { withEnv } from "./test-utils/env-guard.js";

const cleanup: string[] = [];
afterEach(() => {
  for (const path of cleanup.splice(0)) rmSync(path, { force: true, recursive: true });
});

describe("durable plugin logging", () => {
  test("test logs repair their private namespace, not the shared temp directory", async () => {
    const root = mkdtempSync(join(tmpdir(), "aft-shared-log-parent-"));
    cleanup.push(root);
    const shared = join(root, "shared");
    mkdirSync(shared);
    if (process.platform !== "win32") chmodSync(shared, 0o755);
    const before = statSync(shared).mode;
    const env: Record<string, string> = { BUN_TEST: "1", TMPDIR: shared };
    const path = resolvePluginLogPath({ lookup: (name) => env[name] });
    const sink = new RotatingLogSink(path);
    sink.append("test log\n");
    await sink.drain();
    expect(readFileSync(path, "utf8")).toBe("test log\n");
    expect(statSync(shared).mode).toBe(before);
  });
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

  // Every log line went through two mkdir calls before its append. The
  // directory only needs creating once per sink, and again if it disappears.
  test("creates the log directory once, not once per write", async () => {
    const root = mkdtempSync(join(tmpdir(), "aft-durable-log-"));
    cleanup.push(root);
    const path = join(root, "logs", "aft-plugin.log");
    const sink = new RotatingLogSink(path);

    for (let i = 0; i < 10; i++) sink.append(`line ${i}\n`);
    await sink.drain();

    expect(sink.__mkdirCallsForTests()).toBe(2);
    expect(readFileSync(path, "utf8").split("\n")).toHaveLength(11);
  });

  test("recreates a log directory removed between writes", async () => {
    const root = mkdtempSync(join(tmpdir(), "aft-durable-log-"));
    cleanup.push(root);
    const path = join(root, "storage", "logs", "aft-plugin.log");
    const sink = new RotatingLogSink(path);

    sink.append("before\n");
    await sink.drain();
    rmSync(join(root, "storage"), { recursive: true, force: true });
    sink.append("after\n");
    await sink.drain();

    expect(readFileSync(path, "utf8")).toBe("after\n");
    if (process.platform !== "win32") expect(modeOf(join(root, "storage", "logs"))).toBe(0o700);
  });

  // Every OpenCode and Pi process appends to the same plugin log. Two sinks on
  // one path stand in for two processes here.
  test("rotation follows the shared file's real size, not one writer's own count", async () => {
    const root = mkdtempSync(join(tmpdir(), "aft-durable-log-"));
    cleanup.push(root);
    const path = join(root, "logs", "aft-plugin.log");
    const a = new RotatingLogSink(path, { maxBytes: 1000 });
    const b = new RotatingLogSink(path, { maxBytes: 1000 });
    const line = (name: string) => `${name.padEnd(99, ".")}\n`;
    const write = async (sink: RotatingLogSink, name: string) => {
      sink.append(line(name));
      await sink.drain();
      expect(statSync(path).size).toBeLessThanOrEqual(1000);
    };

    for (let i = 0; i < 9; i++) await write(a, `A${i}`);
    await write(b, "B0");
    await write(a, "A9");
    await write(a, "A10");
    await write(b, "B1");

    // B's next line must not rotate again over the backup A just made.
    expect(readFileSync(`${path}.1`, "utf8").startsWith(line("A0"))).toBe(true);
    expect(readFileSync(path, "utf8")).toBe(line("A9") + line("A10") + line("B1"));
  });

  test("a stale rotation keeps late appends from an already-open writer", async () => {
    const root = mkdtempSync(join(tmpdir(), "aft-durable-log-"));
    cleanup.push(root);
    const path = join(root, "aft-plugin.log");
    const sink = new RotatingLogSink(path);
    writeFileSync(path, "full log\n");
    const measured = statSync(path, { bigint: true });

    // The first writer rotates and opens a fresh live file before the stale rotation.
    renameSync(path, `${path}.1`);
    const writer = await open(path, "a");
    try {
      await writer.appendFile("fresh line\n");
      // biome-ignore lint/complexity/useLiteralKeys: Call the private method with the earlier stat to control rotation order.
      await sink["rotate"](measured);
      // Copying the fresh log back would leave this handle pointing at the unlinked original.
      await writer.appendFile("late line\n");
    } finally {
      await writer.close();
    }

    expect(readFileSync(path, "utf8")).toBe("fresh line\nlate line\n");
    expect(readFileSync(`${path}.1`, "utf8")).toBe("full log\n");
    expect(readdirSync(root).sort()).toEqual(["aft-plugin.log", "aft-plugin.log.1"]);
  });

  test("writers that rotate at the same moment keep the backup and every new line", async () => {
    const root = mkdtempSync(join(tmpdir(), "aft-durable-log-"));
    cleanup.push(root);
    const path = join(root, "logs", "aft-plugin.log");
    mkdirSync(join(root, "logs"));
    const full = `${"x".repeat(99)}\n`;
    for (let round = 0; round < 20; round++) {
      writeFileSync(path, full.repeat(10));
      rmSync(`${path}.1`, { force: true });
      const sinks = Array.from({ length: 4 }, () => new RotatingLogSink(path, { maxBytes: 1000 }));
      for (const [index, sink] of sinks.entries()) sink.append(`round ${round} sink ${index}\n`);
      await Promise.all(sinks.map((sink) => sink.drain()));

      expect(readFileSync(`${path}.1`, "utf8")).toBe(full.repeat(10));
      const live = readFileSync(path, "utf8").split("\n").filter(Boolean).sort();
      expect(live).toEqual(sinks.map((_, index) => `round ${round} sink ${index}`));
      expect(readdirSync(join(root, "logs")).sort()).toEqual([
        "aft-plugin.log",
        "aft-plugin.log.1",
      ]);
    }
  });

  test("a failed write pauses the log instead of turning it off", async () => {
    const root = mkdtempSync(join(tmpdir(), "aft-durable-log-"));
    cleanup.push(root);
    const storage = join(root, "storage");
    const path = join(storage, "logs", "aft-plugin.log");
    const sink = new RotatingLogSink(path, { retryDelayMs: 0 });
    const stderrWrite = spyOn(process.stderr, "write").mockImplementation(() => true);
    try {
      // The storage folder is briefly a file, so the first write cannot create it.
      writeFileSync(storage, "obstacle");
      sink.append("lost line\n");
      await sink.drain();
      rmSync(storage, { force: true });
      sink.append("later line\n");
      await sink.drain();

      expect(readFileSync(path, "utf8")).toBe(
        "[aft-plugin] durable log resumed after a write failure; dropped 10 bytes.\nlater line\n",
      );
      expect(stderrWrite).toHaveBeenCalledTimes(1);
    } finally {
      stderrWrite.mockRestore();
    }
  });
});
