import { describe, expect, test } from "bun:test";
import { AftConfigSchema } from "../config";
import { AftConfigSchema as PiConfigSchema } from "../../../pi-plugin/src/config";

describe("project_detection schema", () => {
  for (const [name, schema] of [
    ["opencode", AftConfigSchema],
    ["pi", PiConfigSchema],
  ] as const) {
    test(`${name} project_detection defaults and modes`, () => {
      expect(schema.parse({ project_detection: {} }).project_detection).toEqual({
        mode: "auto",
        overrides: [],
      });
      for (const mode of ["auto", "project", "workspace", "off"]) {
        expect(schema.parse({ project_detection: { mode } }).project_detection?.mode).toBe(mode);
      }
    });

    test(`${name} project_detection rejects malformed policy rather than dropping it`, () => {
      for (const policy of [
        null,
        { mode: "unknown" },
        { overrides: [{ path: "/source" }] },
        { overrides: [{ path: "/source", mode: "project", extra: true }] },
        { extra: true },
      ]) {
        expect(schema.safeParse({ project_detection: policy }).success).toBe(false);
      }
    });
  }
});
