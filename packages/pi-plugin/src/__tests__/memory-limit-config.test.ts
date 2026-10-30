import { expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  AftConfigSchema,
  loadAftConfig,
  resolveBashConfig,
  resolveProjectOverridesForConfigure,
} from "../config";

test("memory ceiling validates MiB and reaches configure without enabling a default", () => {
  expect(resolveBashConfig({ disabled_tools: [] }).memory_limit_mib).toBeUndefined();
  for (const memory_limit_mib of [1, 64, 4294967295]) {
    const config = AftConfigSchema.parse({ disabled_tools: [], bash: { memory_limit_mib } });
    expect(resolveBashConfig(config).memory_limit_mib).toBe(memory_limit_mib);
    expect(resolveProjectOverridesForConfigure(config).bash).toEqual({ memory_limit_mib });
  }
  for (const memory_limit_mib of [0, -1, 1.5, "4GB", true, 4294967296]) {
    expect(AftConfigSchema.safeParse({ bash: { memory_limit_mib } }).success).toBe(false);
  }
});

test("project memory ceilings can tighten but cannot raise or remove a user ceiling", () => {
  const dir = mkdtempSync(join(tmpdir(), "aft-memory-config-"));
  const savedXdg = process.env.XDG_CONFIG_HOME;
  const savedOpenCode = process.env.OPENCODE_CONFIG_DIR;
  process.env.XDG_CONFIG_HOME = dir;
  delete process.env.OPENCODE_CONFIG_DIR;
  const project = join(dir, "project");
  mkdirSync(join(dir, "cortexkit"), { recursive: true });
  mkdirSync(join(project, ".cortexkit"), { recursive: true });
  try {
    writeFileSync(
      join(dir, "cortexkit/aft.jsonc"),
      JSON.stringify({ bash: { memory_limit_mib: 64 } }),
    );
    for (const [bash, expected] of [
      [{ memory_limit_mib: 128 }, 64],
      [{ memory_limit_mib: 32 }, 32],
      [false, 64],
    ]) {
      writeFileSync(join(project, ".cortexkit/aft.jsonc"), JSON.stringify({ bash }));
      expect(resolveBashConfig(loadAftConfig(project)).memory_limit_mib).toBe(expected);
    }
  } finally {
    if (savedXdg === undefined) delete process.env.XDG_CONFIG_HOME;
    else process.env.XDG_CONFIG_HOME = savedXdg;
    if (savedOpenCode === undefined) delete process.env.OPENCODE_CONFIG_DIR;
    else process.env.OPENCODE_CONFIG_DIR = savedOpenCode;
    rmSync(dir, { recursive: true, force: true });
  }
});

test("published schema advertises the same MiB ceiling in base and harness blocks", () => {
  const schema = JSON.parse(
    readFileSync(new URL("../../../../assets/aft.schema.json", import.meta.url), "utf8"),
  );
  const cap = { type: "integer", minimum: 1, maximum: 4294967295 };
  expect(schema.properties.bash.oneOf[1].properties.memory_limit_mib).toMatchObject(cap);
  expect(
    schema.properties.harnesses.additionalProperties.properties.bash.oneOf[1].properties
      .memory_limit_mib,
  ).toMatchObject(cap);
});
