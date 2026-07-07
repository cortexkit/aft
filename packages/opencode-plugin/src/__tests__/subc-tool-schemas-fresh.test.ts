/// <reference path="../bun-test.d.ts" />
import { describe, expect, test } from "bun:test";
import * as fs from "node:fs";
import * as path from "node:path";
import { buildSubcToolSchemasJson, SUBC_BARE_TOOL_NAMES } from "../subc-tool-schemas.js";

const REPO_ROOT = path.resolve(import.meta.dir, "..", "..", "..", "..");
const ARTIFACT_PATH = path.join(REPO_ROOT, "crates", "aft", "src", "subc_tool_schemas.json");

const PLACEHOLDER = JSON.stringify({ type: "object" });

describe("subc tool schemas artifact", () => {
  test("committed artifact matches in-memory generation byte-for-byte", () => {
    const committed = fs.readFileSync(ARTIFACT_PATH, "utf8");
    const fresh = buildSubcToolSchemasJson();
    expect(fresh).toBe(committed);
  });

  test("all bare names present with object schemas", () => {
    expect(SUBC_BARE_TOOL_NAMES).toHaveLength(22);
    const parsed = JSON.parse(fs.readFileSync(ARTIFACT_PATH, "utf8")) as Record<
      string,
      Record<string, unknown>
    >;
    for (const name of SUBC_BARE_TOOL_NAMES) {
      expect(parsed[name]).toBeDefined();
      expect(parsed[name].type).toBe("object");
      expect(typeof parsed[name].description).toBe("string");
      expect((parsed[name].description as string).length).toBeGreaterThan(0);
    }
    expect(Object.keys(parsed).sort()).toEqual([...SUBC_BARE_TOOL_NAMES].sort());
  });

  test("schemas are not bare placeholders (except status empty-object contract)", () => {
    const parsed = JSON.parse(fs.readFileSync(ARTIFACT_PATH, "utf8")) as Record<
      string,
      Record<string, unknown>
    >;
    for (const [name, schema] of Object.entries(parsed)) {
      const serialized = JSON.stringify(schema);
      if (name === "status") {
        expect(schema.properties).toEqual({});
        expect(schema.additionalProperties).toBe(false);
        continue;
      }
      expect(serialized).not.toBe(PLACEHOLDER);
      const props = schema.properties as Record<string, unknown> | undefined;
      expect(props && Object.keys(props).length).toBeGreaterThan(0);
    }
  });
});
