import { describe, expect, test } from "bun:test";
import { findNpmServerByBinary, findNpmServerById, NPM_LSP_TABLE } from "../lsp-npm-table";

describe("npm LSP table", () => {
  test("includes the agreed v0.17.0 packages", () => {
    const ids = NPM_LSP_TABLE.map((s) => s.id);
    expect(ids).toContain("typescript");
    expect(ids).toContain("python");
    expect(ids).toContain("yaml");
    expect(ids).toContain("bash");
    expect(ids).toContain("dockerfile");
    expect(ids).toContain("vue");
    expect(ids).toContain("astro");
    expect(ids).toContain("svelte");
    expect(ids).toContain("biome");
    expect(ids).toContain("php-intelephense");
  });

  test("does NOT include eslint (Pattern E, custom build) or prisma (project-only)", () => {
    const ids = NPM_LSP_TABLE.map((s) => s.id);
    expect(ids).not.toContain("eslint");
    expect(ids).not.toContain("prisma");
  });

  test("ids are unique across the table", () => {
    const ids = NPM_LSP_TABLE.map((s) => s.id);
    expect(new Set(ids).size).toBe(ids.length);
  });

  test("npm package names are unique across the table", () => {
    const npmNames = NPM_LSP_TABLE.map((s) => s.npm);
    expect(new Set(npmNames).size).toBe(npmNames.length);
  });

  test("every entry has at least one extension and a non-empty binary name", () => {
    for (const entry of NPM_LSP_TABLE) {
      expect(entry.extensions.length).toBeGreaterThan(0);
      expect(entry.binary.length).toBeGreaterThan(0);
      expect(entry.npm.length).toBeGreaterThan(0);
    }
  });

  test("findNpmServerById finds a known entry", () => {
    expect(findNpmServerById("typescript")?.npm).toBe("typescript-language-server");
  });

  test("findNpmServerById returns undefined for missing id", () => {
    expect(findNpmServerById("nonexistent-id-zzz")).toBeUndefined();
  });

  test("findNpmServerByBinary finds a known entry by binary name", () => {
    const found = findNpmServerByBinary("docker-langserver");
    expect(found?.npm).toBe("dockerfile-language-server-nodejs");
  });
});
