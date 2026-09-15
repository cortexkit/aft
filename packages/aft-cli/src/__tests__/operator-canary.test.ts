import { describe, expect, test } from "bun:test";
import {
  closeSync,
  mkdirSync,
  mkdtempSync,
  openSync,
  rmSync,
  statSync,
  truncateSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { probeOpenCodeV1Version } from "../setup/host-generation.js";

/**
 * Build a disposable operator home carrying a live-shaped OpenCode data
 * directory. `databaseBytes` is applied with truncate, so a multi-gigabyte
 * database costs no disk space and no time.
 */
function operatorHomeWith(databaseBytes: number): { home: string; database: string } {
  const home = mkdtempSync(join(tmpdir(), "aft-canary-home-"));
  const dataDir = join(home, ".local", "share", "opencode");
  mkdirSync(join(dataDir, "log"), { recursive: true });
  const database = join(dataDir, "opencode.db");
  closeSync(openSync(database, "w"));
  truncateSync(database, databaseBytes);
  writeFileSync(join(dataDir, "log", "opencode.log"), "line\n");
  return { home, database };
}

function fakeSpawn(during?: () => void) {
  return () => {
    during?.();
    return { status: 0, stdout: "1.18.30\n", stderr: "" };
  };
}

describe("operator canary", () => {
  test("probes a host whose database is larger than the runtime can hold", () => {
    // Issue #316: the canary hashed every byte with readFileSync, so a real
    // operator database (reported at 54 GiB, still 4 GiB after pruning) made
    // `aft doctor` exit with ERR_FS_FILE_TOO_LARGE before any diagnostic ran.
    //
    // The assertion is resident memory, not the Node-only 2 GiB throw: the
    // defect is "reads the whole file into memory", the throw is one runtime's
    // symptom of it, and Bun (which runs this suite) has no such limit and
    // would happily allocate the whole file. Peak RSS reds under both runtimes.
    const { home, database } = operatorHomeWith(3 * 1024 * 1024 * 1024);
    try {
      expect(statSync(database).size).toBeGreaterThan(2 * 1024 * 1024 * 1024);
      const before = process.memoryUsage().rss;
      const version = probeOpenCodeV1Version("/does/not/matter", {
        operatorHome: home,
        spawn: fakeSpawn() as never,
      });
      const growth = process.memoryUsage().rss - before;
      expect(version).toBe("1.18.30");
      expect(growth).toBeLessThan(256 * 1024 * 1024);
    } finally {
      rmSync(home, { recursive: true, force: true });
    }
  });

  test("still fails the probe when the oversized database is written during it", () => {
    // The large-file path hashes head, tail, size and mtime rather than every
    // byte, so this asserts the weaker digest still moves on a real write.
    const { home, database } = operatorHomeWith(3 * 1024 * 1024 * 1024);
    try {
      expect(() =>
        probeOpenCodeV1Version("/does/not/matter", {
          operatorHome: home,
          spawn: fakeSpawn(() => {
            truncateSync(database, 3 * 1024 * 1024 * 1024 + 4096);
          }) as never,
        }),
      ).toThrow(/changed the operator database/);
    } finally {
      rmSync(home, { recursive: true, force: true });
    }
  });

  test("fails the probe when a write lands only in the WAL sidecar", () => {
    // SQLite in WAL mode writes the sidecar first; a canary watching only the
    // main database file would miss the write it exists to catch.
    const { home, database } = operatorHomeWith(1024);
    try {
      writeFileSync(`${database}-wal`, "before");
      expect(() =>
        probeOpenCodeV1Version("/does/not/matter", {
          operatorHome: home,
          spawn: fakeSpawn(() => {
            writeFileSync(`${database}-wal`, "after!");
          }) as never,
        }),
      ).toThrow(/changed the operator database/);
    } finally {
      rmSync(home, { recursive: true, force: true });
    }
  });

  test("detects a same-size byte change in a small file", () => {
    const { home } = operatorHomeWith(1024);
    const logFile = join(home, ".local", "share", "opencode", "log", "opencode.log");
    try {
      expect(() =>
        probeOpenCodeV1Version("/does/not/matter", {
          operatorHome: home,
          spawn: fakeSpawn(() => {
            writeFileSync(logFile, "LINE\n");
          }) as never,
        }),
      ).toThrow(/changed the operator database/);
    } finally {
      rmSync(home, { recursive: true, force: true });
    }
  });

  test("passes an untouched operator home", () => {
    const { home } = operatorHomeWith(1024);
    try {
      expect(
        probeOpenCodeV1Version("/does/not/matter", {
          operatorHome: home,
          spawn: fakeSpawn() as never,
        }),
      ).toBe("1.18.30");
    } finally {
      rmSync(home, { recursive: true, force: true });
    }
  });
});
