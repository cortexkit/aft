import { expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { AftConfigSchema, loadAftConfig, resolveProjectOverridesForConfigure } from "../config";

test("published JSON schema advertises the opt-in privacy setting", () => {
  const schema = JSON.parse(
    readFileSync(new URL("../../../../assets/aft.schema.json", import.meta.url), "utf8"),
  );
  expect(schema.properties.bash.oneOf[1].properties.disclaim_privacy).toMatchObject({
    type: "boolean",
    default: false,
  });
});

test("privacy disclaim is boolean and reaches configure without changing the default", () => {
  expect(AftConfigSchema.safeParse({ bash: { disclaim_privacy: "true" } }).success).toBe(false);
  expect(resolveProjectOverridesForConfigure({ disabled_tools: [] }).bash).toBeUndefined();
  for (const disclaim_privacy of [false, true]) {
    const config = AftConfigSchema.parse({ disabled_tools: [], bash: { disclaim_privacy } });
    expect(resolveProjectOverridesForConfigure(config).bash).toEqual({ disclaim_privacy });
  }
});

test("project privacy disclaim can tighten but cannot weaken user policy, even through bash false", () => {
  const dir = mkdtempSync(join(tmpdir(), "aft-privacy-config-"));
  const savedXdg = process.env.XDG_CONFIG_HOME;
  const savedOpenCode = process.env.OPENCODE_CONFIG_DIR;
  process.env.XDG_CONFIG_HOME = dir;
  delete process.env.OPENCODE_CONFIG_DIR;
  const project = join(dir, "project");
  mkdirSync(join(dir, "cortexkit"), { recursive: true });
  mkdirSync(join(project, ".cortexkit"), { recursive: true });
  try {
    for (const user of [false, true]) {
      for (const bash of [{ disclaim_privacy: false }, { disclaim_privacy: true }, false]) {
        writeFileSync(
          join(dir, "cortexkit/aft.jsonc"),
          JSON.stringify({ bash: { disclaim_privacy: user } }),
        );
        writeFileSync(join(project, ".cortexkit/aft.jsonc"), JSON.stringify({ bash }));
        const merged = loadAftConfig(project);
        expect(
          (resolveProjectOverridesForConfigure(merged).bash as { disclaim_privacy?: boolean })
            ?.disclaim_privacy ?? false,
        ).toBe(user || (typeof bash === "object" && bash.disclaim_privacy));
      }
    }
  } finally {
    if (savedXdg === undefined) delete process.env.XDG_CONFIG_HOME;
    else process.env.XDG_CONFIG_HOME = savedXdg;
    if (savedOpenCode === undefined) delete process.env.OPENCODE_CONFIG_DIR;
    else process.env.OPENCODE_CONFIG_DIR = savedOpenCode;
    rmSync(dir, { recursive: true, force: true });
  }
});
