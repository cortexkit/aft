import { describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { spawnSync } from "../child-process.js";
import { isolatedAftEnvironment } from "../test-child-environment.js";
import {
  assertAftDatabaseUnchanged,
  liveAftDatabasePath,
  snapshotAftDatabase,
} from "./test-utils/storage-canary.js";

const DIRECTORY_KEYS = [
  "HOME",
  "USERPROFILE",
  "LOCALAPPDATA",
  "XDG_CONFIG_HOME",
  "XDG_DATA_HOME",
  "XDG_STATE_HOME",
  "XDG_CACHE_HOME",
  "XDG_RUNTIME_DIR",
  "AFT_STORAGE_DIR",
  "AFT_CACHE_DIR",
];

function fixture(run: (root: string) => void): void {
  const root = mkdtempSync(join(tmpdir(), "aft-storage-isolation-"));
  try {
    run(root);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

function executeFixtureSql(path: string, sql: string): void {
  const result = spawnSync(
    process.execPath,
    [
      "-e",
      `
      const { Database } = require("bun:sqlite");
      const db = new Database(process.env.FIXTURE_DB);
      try { db.exec(process.env.FIXTURE_SQL); } finally { db.close(); }
    `,
    ],
    { env: { ...process.env, FIXTURE_DB: path, FIXTURE_SQL: sql }, encoding: "utf8" },
  );
  expect(result.status).toBe(0);
}

function seedDatabase(path: string, version: number): void {
  executeFixtureSql(
    path,
    `
    CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY);
    CREATE TABLE IF NOT EXISTS records (content BLOB);
    DELETE FROM schema_version;
    INSERT INTO schema_version VALUES (${version});
  `,
  );
}

describe("test storage isolation", () => {
  test("shared spawns replace operator directories before the child starts", () => {
    const env: NodeJS.ProcessEnv = { ...process.env, AFT_ALLOW_PRODUCTION_MIGRATION: "1" };
    const operatorStorage = dirname(liveAftDatabasePath());
    for (const key of DIRECTORY_KEYS) env[key] = operatorStorage;
    const result = spawnSync(
      process.execPath,
      ["-e", "process.stdout.write(JSON.stringify(process.env))"],
      { env, encoding: "utf8" },
    );
    expect(result.status).toBe(0);
    const child = JSON.parse(result.stdout);
    for (const key of DIRECTORY_KEYS) {
      expect(child[key]).not.toBe(operatorStorage);
      expect(statSync(child[key]).isDirectory()).toBe(true);
    }
    expect(child.AFT_ALLOW_PRODUCTION_MIGRATION).toBeUndefined();
  });

  test("storage canary resolves the OS account root rather than the throwaway HOME", () => {
    const path = liveAftDatabasePath();
    expect(path.startsWith(process.env.HOME!)).toBe(false);
  });

  test("shared spawn directories exist and isolated fixture overrides survive", () =>
    fixture((root) => {
      const env = isolatedAftEnvironment(root);
      const result = spawnSync(
        process.execPath,
        ["-e", "process.stdout.write(JSON.stringify(process.env))"],
        { env, encoding: "utf8" },
      );
      expect(result.status).toBe(0);
      const child = JSON.parse(result.stdout);
      for (const key of DIRECTORY_KEYS) {
        expect(child[key]).toBe(env[key]);
        expect(statSync(child[key]).isDirectory()).toBe(true);
      }
    }));

  test("storage canary detects a schema version bump", () =>
    fixture((root) => {
      const path = join(root, "aft.db");
      seedDatabase(path, 13);
      const before = snapshotAftDatabase(path);
      expect(before?.schemaVersion).toBe(13);
      expect(() => assertAftDatabaseUnchanged(path, before)).not.toThrow();
      seedDatabase(path, 14);
      expect(snapshotAftDatabase(path)?.schemaVersion).toBe(14);
      expect(() => assertAftDatabaseUnchanged(path, before)).toThrow("live AFT database changed");
    }));

  test("storage canary detects a newly created table at the same version", () =>
    fixture((root) => {
      const path = join(root, "aft.db");
      seedDatabase(path, 14);
      const before = snapshotAftDatabase(path);
      expect(before?.objects.map((object) => object.name)).toEqual(["records", "schema_version"]);
      executeFixtureSql(path, "CREATE TABLE call_ledger (id INTEGER);");
      expect(snapshotAftDatabase(path)?.schemaVersion).toBe(14);
      expect(() => assertAftDatabaseUnchanged(path, before)).toThrow("live AFT database changed");
    }));

  test("storage canary detects a newly created index at the same version", () =>
    fixture((root) => {
      const path = join(root, "aft.db");
      seedDatabase(path, 14);
      const before = snapshotAftDatabase(path);
      executeFixtureSql(path, "CREATE INDEX records_content ON records (content);");
      expect(snapshotAftDatabase(path)?.schemaVersion).toBe(14);
      expect(() => assertAftDatabaseUnchanged(path, before)).toThrow("live AFT database changed");
    }));

  test("storage canary permits ordinary concurrent record writes", () =>
    fixture((root) => {
      const path = join(root, "aft.db");
      seedDatabase(path, 14);
      const before = snapshotAftDatabase(path);
      executeFixtureSql(path, "INSERT INTO records VALUES (zeroblob(16384));");
      expect(() => assertAftDatabaseUnchanged(path, before)).not.toThrow();
    }));

  test("storage canary detects creation of a previously absent database", () =>
    fixture((root) => {
      const path = join(root, "aft.db");
      expect(snapshotAftDatabase(path)).toBeNull();
      seedDatabase(path, 14);
      expect(() => assertAftDatabaseUnchanged(path, null)).toThrow("live AFT database changed");
    }));

  test("storage canary refuses an unreadable database instead of skipping it", () =>
    fixture((root) => {
      const path = join(root, "aft.db");
      writeFileSync(path, "not sqlite");
      expect(() => snapshotAftDatabase(path)).toThrow("read-only storage canary failed");
    }));
});
