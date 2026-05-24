import { afterEach, describe, expect, it } from "bun:test";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { AftConfigSchema } from "../src/config.js";

const packageRoot = fileURLToPath(new URL("../", import.meta.url));
const tempRoots = new Set<string>();

function createConfigFixture() {
  const root = mkdtempSync(join(tmpdir(), "aft-disabled-tools-tests-"));
  tempRoots.add(root);

  const xdgConfigHome = join(root, "xdg-config");
  const userConfigDir = join(xdgConfigHome, "opencode");
  const projectDirectory = join(root, "project");
  const projectConfigDir = join(projectDirectory, ".opencode");

  mkdirSync(userConfigDir, { recursive: true });
  mkdirSync(projectConfigDir, { recursive: true });

  return {
    root,
    xdgConfigHome,
    projectDirectory,
    userConfigPath: join(userConfigDir, "aft.jsonc"),
    projectConfigPath: join(projectConfigDir, "aft.jsonc"),
  };
}

function runConfigLoader(projectDirectory: string, env: Record<string, string>) {
  const script = `
    import { loadAftConfig } from "./src/config.ts";
    console.log(JSON.stringify(loadAftConfig(process.env.PROJECT_DIR)));
  `;
  const result = spawnSync(process.execPath, ["-e", script], {
    cwd: packageRoot,
    env: { ...process.env, ...env, PROJECT_DIR: projectDirectory },
    encoding: "utf8",
  });

  expect(result.error).toBeUndefined();
  expect(result.status).toBe(0);

  return {
    stdout: result.stdout.trim(),
    stderr: result.stderr.trim(),
  };
}

afterEach(() => {
  for (const root of tempRoots) {
    rmSync(root, { recursive: true, force: true });
  }
  tempRoots.clear();
});

describe("AftConfigSchema — disabled_tools validation", () => {
  it("accepts a valid disabled_tools array of strings", () => {
    const result = AftConfigSchema.safeParse({ disabled_tools: ["aft_navigate"] });
    expect(result.success).toBe(true);
    if (result.success) {
      expect(result.data.disabled_tools).toEqual(["aft_navigate"]);
    }
  });

  it("rejects disabled_tools containing non-string values", () => {
    const result = AftConfigSchema.safeParse({ disabled_tools: [123] });
    expect(result.success).toBe(false);
    if (!result.success) {
      const issue = result.error.issues.find((i) => i.path[0] === "disabled_tools");
      expect(issue).toBeDefined();
    }
  });

  it("accepts an empty disabled_tools array", () => {
    const result = AftConfigSchema.safeParse({ disabled_tools: [] });
    expect(result.success).toBe(true);
    if (result.success) {
      expect(result.data.disabled_tools).toEqual([]);
    }
  });

  it("accepts omitted disabled_tools (optional field)", () => {
    const result = AftConfigSchema.safeParse({ format_on_edit: true });
    expect(result.success).toBe(true);
    if (result.success) {
      expect(result.data.disabled_tools).toBeUndefined();
    }
  });
});

describe("loadAftConfig — disabled_tools merge behavior", () => {
  it("unions disabled_tools from user and project configs", () => {
    const fixture = createConfigFixture();
    writeFileSync(fixture.userConfigPath, JSON.stringify({ disabled_tools: ["aft_navigate"] }));
    writeFileSync(fixture.projectConfigPath, JSON.stringify({ disabled_tools: ["aft_refactor"] }));

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: join(fixture.root, "home"),
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    const config = JSON.parse(result.stdout);
    expect(config.disabled_tools).toContain("aft_navigate");
    expect(config.disabled_tools).toContain("aft_refactor");
    expect(config.disabled_tools).toHaveLength(2);
  });

  it("deduplicates duplicate entries across user and project configs", () => {
    const fixture = createConfigFixture();
    writeFileSync(
      fixture.userConfigPath,
      JSON.stringify({ disabled_tools: ["aft_navigate", "aft_refactor"] }),
    );
    writeFileSync(fixture.projectConfigPath, JSON.stringify({ disabled_tools: ["aft_refactor"] }));

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: join(fixture.root, "home"),
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    const config = JSON.parse(result.stdout);
    expect(config.disabled_tools).toContain("aft_navigate");
    expect(config.disabled_tools).toContain("aft_refactor");
    expect(config.disabled_tools).toHaveLength(2);
  });

  it("produces an empty array when both configs have empty disabled_tools", () => {
    const fixture = createConfigFixture();
    writeFileSync(fixture.userConfigPath, JSON.stringify({ disabled_tools: [] }));
    writeFileSync(fixture.projectConfigPath, JSON.stringify({ disabled_tools: [] }));

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: join(fixture.root, "home"),
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    const config = JSON.parse(result.stdout);
    // Both configs have empty arrays — merged result is also empty (no tools filtered)
    expect(Array.isArray(config.disabled_tools)).toBe(true);
    expect(config.disabled_tools).toHaveLength(0);
  });

  it("inherits user-level disabled_tools when project config has none", () => {
    const fixture = createConfigFixture();
    writeFileSync(fixture.userConfigPath, JSON.stringify({ disabled_tools: ["aft_navigate"] }));
    writeFileSync(fixture.projectConfigPath, JSON.stringify({ format_on_edit: true }));

    const result = runConfigLoader(fixture.projectDirectory, {
      HOME: join(fixture.root, "home"),
      XDG_CONFIG_HOME: fixture.xdgConfigHome,
    });

    const config = JSON.parse(result.stdout);
    expect(config.disabled_tools).toEqual(["aft_navigate"]);
  });
});
