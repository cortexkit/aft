/// <reference path="../bun-test.d.ts" />

import { beforeEach, describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  AFT_SCHEMA_URL,
  detectJsoncFile,
  ensureAftSchemaUrl,
  readJsoncFile,
  writeJsoncFile,
} from "../lib/jsonc.js";

describe("jsonc", () => {
  let dir: string;

  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), "aft-cli-jsonc-"));
    mkdirSync(dir, { recursive: true });
  });

  test("detectJsoncFile returns .jsonc when both exist", () => {
    writeFileSync(join(dir, "aft.jsonc"), "{}");
    writeFileSync(join(dir, "aft.json"), "{}");
    const detected = detectJsoncFile(dir, "aft");
    expect(detected.format).toBe("jsonc");
  });

  test("detectJsoncFile falls back to .json", () => {
    writeFileSync(join(dir, "aft.json"), "{}");
    const detected = detectJsoncFile(dir, "aft");
    expect(detected.format).toBe("json");
  });

  test("detectJsoncFile returns none when neither exists", () => {
    const detected = detectJsoncFile(dir, "aft");
    expect(detected.format).toBe("none");
    expect(detected.path.endsWith(".json")).toBe(true);
  });

  test("readJsoncFile parses comment-stripped JSON", () => {
    const path = join(dir, "x.jsonc");
    writeFileSync(path, '// a comment\n{\n  "key": 1\n}\n');
    const { value, error } = readJsoncFile(path);
    expect(error).toBeUndefined();
    expect(value?.key).toBe(1);
  });

  test("readJsoncFile returns error on bad content", () => {
    const path = join(dir, "bad.jsonc");
    writeFileSync(path, "{not json}");
    const { value, error } = readJsoncFile(path);
    expect(value).toBeNull();
    expect(error).toBeDefined();
  });

  test("writeJsoncFile creates parent dirs", () => {
    const path = join(dir, "nested", "deep", "out.json");
    writeJsoncFile(path, { foo: 1 });
    const raw = readFileSync(path, "utf-8");
    expect(JSON.parse(raw)).toEqual({ foo: 1 });
  });
});

describe("ensureAftSchemaUrl", () => {
  let dir: string;

  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), "aft-cli-schema-"));
    mkdirSync(dir, { recursive: true });
  });

  test("creates missing aft.json with $schema only", () => {
    const path = join(dir, "missing", "aft.json");
    const result = ensureAftSchemaUrl(path, "none");
    expect(result.action).toBe("added");
    expect(existsSync(path)).toBe(true);
    expect(JSON.parse(readFileSync(path, "utf-8"))).toEqual({ $schema: AFT_SCHEMA_URL });
  });

  test("adds $schema to existing aft.jsonc preserving fields", () => {
    const path = join(dir, "aft.jsonc");
    writeFileSync(path, '{\n  "format_on_edit": true\n}\n');
    const result = ensureAftSchemaUrl(path, "jsonc");
    expect(result.action).toBe("added");
    const parsed = JSON.parse(readFileSync(path, "utf-8"));
    expect(parsed.$schema).toBe(AFT_SCHEMA_URL);
    expect(parsed.format_on_edit).toBe(true);
  });

  test("preserves jsonc comments when adding $schema", () => {
    const path = join(dir, "aft.jsonc");
    writeFileSync(path, '// inline comment\n{\n  "format_on_edit": true\n}\n');
    ensureAftSchemaUrl(path, "jsonc");
    const raw = readFileSync(path, "utf-8");
    expect(raw).toContain("// inline comment");
    expect(raw).toContain("$schema");
  });

  test("no-op when $schema already matches", () => {
    const path = join(dir, "aft.json");
    writeFileSync(path, JSON.stringify({ $schema: AFT_SCHEMA_URL, foo: 1 }));
    const result = ensureAftSchemaUrl(path, "json");
    expect(result.action).toBe("unchanged");
  });

  test("updates $schema when it points elsewhere", () => {
    const path = join(dir, "aft.json");
    writeFileSync(path, JSON.stringify({ $schema: "https://example.com/old", foo: 1 }));
    const result = ensureAftSchemaUrl(path, "json");
    expect(result.action).toBe("updated");
    const parsed = JSON.parse(readFileSync(path, "utf-8"));
    expect(parsed.$schema).toBe(AFT_SCHEMA_URL);
    expect(parsed.foo).toBe(1);
  });

  test("throws when file exists but is unparseable", () => {
    const path = join(dir, "aft.jsonc");
    writeFileSync(path, "{not json");
    expect(() => ensureAftSchemaUrl(path, "jsonc")).toThrow();
  });
});
