import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { AftConfigSchema as PiConfigSchema } from "../../../pi-plugin/src/config.js";
import { AftConfigSchema } from "../config";

test("task scope ceiling validates in both plugin schemas without adding defaults", () => {
  for (const schema of [AftConfigSchema, PiConfigSchema]) {
    for (const value of [1, 86400, 4294967295]) {
      expect(schema.parse({ bash: { linux_scope_max_runtime_secs: value } })).toMatchObject({
        bash: { linux_scope_max_runtime_secs: value },
      });
    }
    for (const value of [0, -1, 1.5, 4294967296, true, "86400"]) {
      expect(schema.safeParse({ bash: { linux_scope_max_runtime_secs: value } }).success).toBe(
        false,
      );
    }
    expect(schema.parse({ bash: {} }).bash).toEqual({});
  }
});

test("published task scope defaults agree in base and harness blocks", () => {
  const schema = JSON.parse(
    readFileSync(new URL("../../../../assets/aft.schema.json", import.meta.url), "utf8"),
  );
  for (const properties of [
    schema.properties.bash.oneOf[1].properties,
    schema.properties.harnesses.additionalProperties.properties.bash.oneOf[1].properties,
  ]) {
    expect(properties.linux_scope.default).toBe(true);
    expect(properties.linux_scope_max_runtime_secs).toMatchObject({
      type: "integer",
      minimum: 1,
      maximum: 4294967295,
      default: 86400,
    });
  }
});
