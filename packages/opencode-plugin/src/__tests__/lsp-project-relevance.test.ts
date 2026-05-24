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
    tmp = join(tmpdir(), `aft-pjdep-${Date.now()}-${Math.random()}`);
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

  test("returns false when package.json is a JSON primitive", () => {
    writeFileSync(join(tmp, "package.json"), '"a string"');
    expect(hasPackageJsonDep(tmp, ["vue"])).toBe(false);
  });

  test("detects deps in dependencies", () => {
    writeFileSync(join(tmp, "package.json"), JSON.stringify({ dependencies: { vue: "^3.4.0" } }));
    expect(hasPackageJsonDep(tmp, ["vue"])).toBe(true);
  });

  test("detects deps in devDependencies", () => {
    writeFileSync(
      join(tmp, "package.json"),
      JSON.stringify({ devDependencies: { astro: "^4.0.0" } }),
    );
    expect(hasPackageJsonDep(tmp, ["astro"])).toBe(true);
  });

  test("detects deps in peerDependencies", () => {
    writeFileSync(
      join(tmp, "package.json"),
      JSON.stringify({ peerDependencies: { svelte: "^4.0.0" } }),
    );
    expect(hasPackageJsonDep(tmp, ["svelte"])).toBe(true);
  });

  test("matches when ANY depName from list is present", () => {
    writeFileSync(join(tmp, "package.json"), JSON.stringify({ dependencies: { nuxt: "^3.0.0" } }));
    // vue spec lists ["vue", "@vue/runtime-core", "nuxt"] — match on third.
    expect(hasPackageJsonDep(tmp, ["vue", "@vue/runtime-core", "nuxt"])).toBe(true);
  });

  test("returns false when no listed dep is present", () => {
    writeFileSync(join(tmp, "package.json"), JSON.stringify({ dependencies: { react: "^18" } }));
    expect(hasPackageJsonDep(tmp, ["vue", "@vue/runtime-core", "nuxt"])).toBe(false);
  });

  test("ignores non-object deps fields", () => {
    writeFileSync(
      join(tmp, "package.json"),
      // Garbage shape: dependencies is a string, not an object. Should not throw.
      JSON.stringify({ dependencies: "this is wrong", devDependencies: { vue: "^3" } }),
    );
    expect(hasPackageJsonDep(tmp, ["vue"])).toBe(true);
  });
});

describe("Vue/Astro/Svelte specs have detection signals (GitHub #48)", () => {
  // Regression: vue/astro/svelte previously had only `extensions` and relied
  // entirely on the bounded extension walk, which fails for monorepo layouts.
  // The fix adds rootMarkers AND packageJsonDeps to each.

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

  test("Vite-based Vue project (no rootMarker, vue dep only) detects via packageJsonDeps", () => {
    // Simulates the GitHub #48 failure: user clones a Vite + Vue starter from
    // GitHub. There's no vue.config.* or nuxt.config.*, only vite.config.ts
    // (which is also valid for React, Svelte, Vanilla — not Vue-specific).
    // Without packageJsonDeps, isProjectRelevant would have to rely on the
    // extension walk and could miss .vue files in monorepo layouts.
    const tmp = join(tmpdir(), `aft-vite-vue-${Date.now()}-${Math.random()}`);
    mkdirSync(tmp, { recursive: true });
    try {
      writeFileSync(
        join(tmp, "package.json"),
        JSON.stringify({
          dependencies: { vue: "^3.4.0", "vue-router": "^4.0.0" },
          devDependencies: { vite: "^5.0.0", "@vitejs/plugin-vue": "^5.0.0" },
        }),
      );
      // Deliberately do NOT create vue.config.* — only vite.config.ts.
      writeFileSync(join(tmp, "vite.config.ts"), "export default {};\n");

      const spec = findNpmServerById("vue");
      expect(spec).toBeDefined();
      // Vue spec's rootMarkers must NOT match (vite.config.* not in the list).
      expect(hasRootMarker(tmp, spec?.rootMarkers)).toBe(false);
      // But packageJsonDeps SHOULD match — that's the new detection path.
      expect(hasPackageJsonDep(tmp, spec?.packageJsonDeps)).toBe(true);
    } finally {
      rmSync(tmp, { recursive: true, force: true });
    }
  });
});

describe("relevantExtensionsInProject still works as before", () => {
  // Sanity: confirm the third signal (extension walk) still functions; the new
  // packageJsonDeps check is additive, not a replacement.
  test("walks project and finds .vue file at depth 2", () => {
    const tmp = join(tmpdir(), `aft-extwalk-${Date.now()}-${Math.random()}`);
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
  // Sanity: every spec has the required base fields after the schema extension.
  test.each(
    NPM_LSP_TABLE.map((s) => [s.id, s] as const),
  )("%s has id + npm + binary + extensions", (_id, spec) => {
    expect(spec.id).toBeTruthy();
    expect(spec.npm).toBeTruthy();
    expect(spec.binary).toBeTruthy();
    expect(spec.extensions.length).toBeGreaterThan(0);
  });
});
