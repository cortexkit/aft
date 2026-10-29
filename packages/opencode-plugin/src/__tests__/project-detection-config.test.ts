import { describe, expect, test } from "bun:test";
import { AftConfigSchema } from "../config";

describe("project_detection schema", () => {
  test("project_detection defaults and modes", () => {
    expect(AftConfigSchema.parse({ project_detection: {} }).project_detection).toEqual({
      mode: "auto",
      overrides: [],
    });
    for (const mode of ["auto", "project", "workspace", "off"]) {
      expect(AftConfigSchema.parse({ project_detection: { mode } }).project_detection?.mode).toBe(
        mode,
      );
    }
  });

  test("project_detection rejects malformed policy rather than dropping it", () => {
    for (const policy of [
      null,
      { mode: "unknown" },
      { overrides: [{ path: "/source" }] },
      { overrides: [{ path: "/source", mode: "project", extra: true }] },
      { extra: true },
    ]) {
      expect(AftConfigSchema.safeParse({ project_detection: policy }).success).toBe(false);
    }
  });
});
