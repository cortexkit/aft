import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { findNpmServerById, NPM_LSP_TABLE } from "../lsp-npm-table";
import {
  hasPackageJsonDep,
  hasRootMarker,
  relevantExtensionsInProject,
} from "../lsp-project-relevance";

describe("hasPackageJsonDep (GitHub #48)", () => {
  let tmp: string;

  beforeEach(() => {
    tmp = join(tmpdir(), `aft-pi-pjdep-${Date.now()}-${Math.random()}`);
    mkdirSync(tmp, { recursive: true });
  });

  afterEach(() => {
    rmSync(tmp, { recursive: true, force: true });
  });

  test("returns false when depNames is undefined or empty", () => {
    writeFileSync(join(tmp, "package.json"), JSON.stringify({ dependencies: { vue: "^3" } }));
    expect(hasPackageJsonDep(tmp, undefined)).toBe(false);
    expect(hasPackageJsonDep(tmp, [])).toBe(false);
  });

  test("returns false when no package.json exists", () => {
    expect(hasPackageJsonDep(tmp, ["vue"])).toBe(false);
  });

  test("returns false on invalid JSON without throwing", () => {
    writeFileSync(join(tmp, "package.json"), "{ not valid");
    expect(hasPackageJsonDep(tmp, ["vue"])).toBe(false);
  });

  test("detects deps in dependencies, devDependencies, peerDependencies", () => {
    writeFileSync(
      join(tmp, "package.json"),
      JSON.stringify({
        dependencies: { vue: "^3" },
        devDependencies: { astro: "^4" },
        peerDependencies: { svelte: "^4" },
      }),
    );
    expect(hasPackageJsonDep(tmp, ["vue"])).toBe(true);
    expect(hasPackageJsonDep(tmp, ["astro"])).toBe(true);
    expect(hasPackageJsonDep(tmp, ["svelte"])).toBe(true);
  });

  test("matches when ANY depName from list is present", () => {
    writeFileSync(join(tmp, "package.json"), JSON.stringify({ dependencies: { nuxt: "^3.0.0" } }));
    expect(hasPackageJsonDep(tmp, ["vue", "@vue/runtime-core", "nuxt"])).toBe(true);
  });

  test("returns false when no listed dep is present", () => {
    writeFileSync(join(tmp, "package.json"), JSON.stringify({ dependencies: { react: "^18" } }));
    expect(hasPackageJsonDep(tmp, ["vue", "@vue/runtime-core", "nuxt"])).toBe(false);
  });
});

describe("Vue/Astro/Svelte specs have detection signals (GitHub #48)", () => {
  test("vue spec has rootMarkers + packageJsonDeps", () => {
    const spec = findNpmServerById("vue");
    expect(spec).toBeDefined();
    expect(spec?.rootMarkers).toContain("vue.config.ts");
    expect(spec?.rootMarkers).toContain("nuxt.config.ts");
    expect(spec?.packageJsonDeps).toContain("vue");
    expect(spec?.packageJsonDeps).toContain("nuxt");
  });

  test("astro spec has rootMarkers + packageJsonDeps", () => {
    const spec = findNpmServerById("astro");
    expect(spec).toBeDefined();
    expect(spec?.rootMarkers).toContain("astro.config.mjs");
    expect(spec?.packageJsonDeps).toContain("astro");
  });

  test("svelte spec has rootMarkers + packageJsonDeps", () => {
    const spec = findNpmServerById("svelte");
    expect(spec).toBeDefined();
    expect(spec?.rootMarkers).toContain("svelte.config.js");
    expect(spec?.packageJsonDeps).toContain("svelte");
    expect(spec?.packageJsonDeps).toContain("@sveltejs/kit");
  });

  test("Vite-based Vue project detects via packageJsonDeps when no rootMarker matches", () => {
    const tmp = join(tmpdir(), `aft-pi-vite-vue-${Date.now()}-${Math.random()}`);
    mkdirSync(tmp, { recursive: true });
    try {
      writeFileSync(join(tmp, "package.json"), JSON.stringify({ dependencies: { vue: "^3.4.0" } }));
      writeFileSync(join(tmp, "vite.config.ts"), "export default {};\n");

      const spec = findNpmServerById("vue");
      expect(spec).toBeDefined();
      expect(hasRootMarker(tmp, spec?.rootMarkers)).toBe(false);
      expect(hasPackageJsonDep(tmp, spec?.packageJsonDeps)).toBe(true);
    } finally {
      rmSync(tmp, { recursive: true, force: true });
    }
  });
});

describe("relevantExtensionsInProject still works as before", () => {
  test("walks project and finds .vue file at depth 2", () => {
    const tmp = join(tmpdir(), `aft-pi-extwalk-${Date.now()}-${Math.random()}`);
    mkdirSync(join(tmp, "src", "components"), { recursive: true });
    try {
      writeFileSync(join(tmp, "src", "components", "App.vue"), "<template></template>\n");
      const extMap: Record<string, string[]> = { vue: ["vue"] };
      const found = relevantExtensionsInProject(tmp, extMap);
      expect(found.has("vue")).toBe(true);
    } finally {
      rmSync(tmp, { recursive: true, force: true });
    }
  });
});

describe("NPM_LSP_TABLE: all specs are still valid", () => {
  test.each(
    NPM_LSP_TABLE.map((s) => [s.id, s] as const),
  )("%s has id + npm + binary + extensions", (_id, spec) => {
    expect(spec.id).toBeTruthy();
    expect(spec.npm).toBeTruthy();
    expect(spec.binary).toBeTruthy();
    expect(spec.extensions.length).toBeGreaterThan(0);
  });
});
