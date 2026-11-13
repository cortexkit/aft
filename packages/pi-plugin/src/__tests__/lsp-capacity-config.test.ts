import { expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { AftConfigSchema, loadAftConfig, resolveProjectOverridesForConfigure } from "../config";

test("lsp.max_servers validates and projects can only lower the user ceiling", () => {
  const dir = mkdtempSync(join(tmpdir(), "aft-lsp-capacity-"));
  const savedXdg = process.env.XDG_CONFIG_HOME;
  const savedOpenCode = process.env.OPENCODE_CONFIG_DIR;
  process.env.XDG_CONFIG_HOME = dir;
  delete process.env.OPENCODE_CONFIG_DIR;
  const project = join(dir, "project");
  mkdirSync(join(dir, "cortexkit"), { recursive: true });
  mkdirSync(join(project, ".cortexkit"), { recursive: true });
  try {
    writeFileSync(join(dir, "cortexkit/aft.jsonc"), JSON.stringify({ lsp: { max_servers: 3 } }));
    for (const [max_servers, expected] of [
      [23, 3],
      [2, 2],
      [0, 0],
    ]) {
      writeFileSync(
        join(project, ".cortexkit/aft.jsonc"),
        JSON.stringify({ lsp: { max_servers } }),
      );
      const config = loadAftConfig(project);
      expect(config.lsp?.max_servers).toBe(expected);
      expect(resolveProjectOverridesForConfigure(config).lsp_max_servers).toBe(expected);
    }
    for (const max_servers of [-1, 1.5, "8"]) {
      expect(AftConfigSchema.safeParse({ lsp: { max_servers } }).success).toBe(false);
    }
  } finally {
    if (savedXdg === undefined) delete process.env.XDG_CONFIG_HOME;
    else process.env.XDG_CONFIG_HOME = savedXdg;
    if (savedOpenCode === undefined) delete process.env.OPENCODE_CONFIG_DIR;
    else process.env.OPENCODE_CONFIG_DIR = savedOpenCode;
    rmSync(dir, { recursive: true, force: true });
  }
});
