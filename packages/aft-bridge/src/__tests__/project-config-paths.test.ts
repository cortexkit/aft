/// <reference path="../bun-test.d.ts" />
import { expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { readConfigTiers } from "../config-tiers.js";
import { legacyProjectConfigLocationNotice, resolveProjectConfigReadPath } from "../paths.js";

const fixtures = JSON.parse(
  readFileSync(
    new URL(
      "../../../../crates/aft/tests/fixtures/config_locations/project_paths.json",
      import.meta.url,
    ),
    "utf8",
  ),
) as {
  cases: Array<{
    name: string;
    harness: "opencode" | "pi" | null;
    files: string[];
    selected: string;
  }>;
};

test("legacy project path selection matches the shared Rust/TypeScript fixtures without writes", () => {
  for (const fixture of fixtures.cases) {
    const root = mkdtempSync(join(tmpdir(), "aft-project-locations-"));
    const text = '{\n // committed legacy config\n "experimental_bash_compress": false\n}\n';
    try {
      for (const file of fixture.files) {
        const path = join(root, file);
        mkdirSync(dirname(path), { recursive: true });
        writeFileSync(path, text);
      }
      const path = resolveProjectConfigReadPath(root, fixture.harness ?? undefined);
      expect(path, fixture.name).toBe(join(root, fixture.selected));
      const tiers = readConfigTiers({
        userConfigPath: join(root, "missing-user.jsonc"),
        projectConfigPath: path,
      });
      if (fixture.files.length > 0) {
        expect(tiers).toEqual([{ tier: "project", source: path, doc: text }]);
      } else {
        expect(tiers).toEqual([]);
      }
      for (const file of fixture.files) expect(readFileSync(join(root, file), "utf8")).toBe(text);
      expect(existsSync(join(root, ".cortexkit/aft.jsonc"))).toBe(
        fixture.files.includes(".cortexkit/aft.jsonc"),
      );
      const notice = legacyProjectConfigLocationNotice(root, path);
      if (fixture.selected.startsWith(".cortexkit/")) expect(notice).toBeNull();
      else {
        expect(notice).toContain(path);
        expect(notice).toContain(join(root, ".cortexkit/aft.jsonc"));
        expect(notice).toContain("doctor --fix");
      }
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  }
});
