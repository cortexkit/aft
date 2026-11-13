/// <reference path="../bun-test.d.ts" />

/**
 * Feature-config registration policy on the Pi and OMP adapters.
 *
 * Every case loads real config files through `loadAftConfig`, so the legacy
 * translation, the absent-base default and explicit-list presence are
 * exercised end to end. A tool registers exactly when its canonical name is
 * absent from the resolved `disabled_tools`; the Pi/OMP adapter additionally
 * has no implementation for the tools listed in `ADAPTER_UNIMPLEMENTED_TOOLS`.
 */

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  ADAPTER_UNIMPLEMENTED_TOOLS,
  CANONICAL_TOOLS,
  SEMANTIC_COST_NOTICE,
} from "@cortexkit/aft-bridge";

import {
  type AftConfig,
  getConfigLoadNotices,
  getConfigValidationErrors,
  loadAftConfig,
} from "../config.js";
import { registerPiToolSurface, resolvePiToolSurface } from "../tool-registration.js";
import { makeMockApi, makeMockBridge, makePluginContext } from "./tool-test-utils.js";

const HOSTS = ["apply_patch", "bash", "edit", "glob", "grep", "read", "write"];

const CASES: Array<{ name: string; user: unknown; disabled: string[] }> = [
  { name: "no config", user: undefined, disabled: ["aft_delete", "aft_move"] },
  { name: "{}", user: {}, disabled: ["aft_delete", "aft_move"] },
  { name: "disabled_tools: []", user: { disabled_tools: [] }, disabled: [] },
  {
    name: 'disabled_tools: ["aft_search"]',
    user: { disabled_tools: ["aft_search"] },
    disabled: ["aft_search"],
  },
  { name: 'legacy tool_surface: "all"', user: { tool_surface: "all" }, disabled: [] },
  {
    name: 'legacy tool_surface: "recommended"',
    user: { tool_surface: "recommended" },
    disabled: ["aft_callgraph", "aft_delete", "aft_move"],
  },
  {
    name: 'legacy tool_surface: "minimal"',
    user: { tool_surface: "minimal" },
    disabled: CANONICAL_TOOLS.filter(
      (tool) => !["aft_outline", "aft_zoom", "aft_safety"].includes(tool),
    ),
  },
  {
    name: "legacy hoist_builtin_tools: false",
    user: { hoist_builtin_tools: false },
    disabled: ["aft_delete", "aft_move", ...HOSTS],
  },
];

let root: string;
let previousCwd: string;
let savedEnv: Record<string, string | undefined>;

beforeEach(() => {
  root = mkdtempSync(join(tmpdir(), "aft-pi-feature-registration-"));
  previousCwd = process.cwd();
  savedEnv = { HOME: process.env.HOME, XDG_CONFIG_HOME: process.env.XDG_CONFIG_HOME };
  process.env.HOME = join(root, "home");
  process.env.XDG_CONFIG_HOME = join(root, "xdg");
});

afterEach(() => {
  process.chdir(previousCwd);
  for (const [key, value] of Object.entries(savedEnv)) {
    if (value === undefined) delete process.env[key];
    else process.env[key] = value;
  }
  rmSync(root, { recursive: true, force: true });
});

function loadWithUserConfig(user: unknown): AftConfig {
  const project = join(root, "project");
  mkdirSync(project, { recursive: true });
  if (user !== undefined) {
    mkdirSync(join(root, "xdg", "cortexkit"), { recursive: true });
    writeFileSync(join(root, "xdg", "cortexkit", "aft.jsonc"), JSON.stringify(user));
  }
  return loadAftConfig(project);
}

function registeredNames(config: AftConfig, harness: "pi" | "omp"): string[] {
  const { api, tools } = makeMockApi();
  const { bridge } = makeMockBridge();
  const ctx = makePluginContext(bridge, { config });
  registerPiToolSurface(api, ctx, resolvePiToolSurface(config), harness);
  return [...tools.keys()].sort();
}

function expected(disabled: readonly string[], harness: "pi" | "omp"): string[] {
  const unimplemented: readonly string[] = ADAPTER_UNIMPLEMENTED_TOOLS[harness];
  return CANONICAL_TOOLS.filter(
    (tool) => !disabled.includes(tool) && !unimplemented.includes(tool),
  ).sort();
}

describe("Pi/OMP feature-config registration", () => {
  for (const harness of ["pi", "omp"] as const) {
    for (const { name, user, disabled } of CASES) {
      test(`${harness} registers canonical tools minus resolved disables: ${name}`, () => {
        const config = loadWithUserConfig(user);
        expect(config.disabled_tools).toEqual([...disabled].sort());
        expect(registeredNames(config, harness)).toEqual(expected(disabled, harness));
      });
    }
  }

  test("the only per-adapter difference is the recorded unimplemented set", () => {
    const config = loadWithUserConfig({ disabled_tools: [] });
    const missing = CANONICAL_TOOLS.filter((tool) => !registeredNames(config, "pi").includes(tool));
    expect(missing).toEqual(["apply_patch", "glob"]);
    expect(ADAPTER_UNIMPLEMENTED_TOOLS.pi).toEqual(["apply_patch", "glob"]);
    expect(ADAPTER_UNIMPLEMENTED_TOOLS.omp).toEqual(["apply_patch", "glob"]);
  });

  test("retired keys in the user file translate and are left to the engine to rewrite", () => {
    const user = {
      disabled_tools: ["aft_glob"],
      search_index: true,
      semantic_search: false,
      callgraph_store: false,
      gh_shim: { enabled: false },
      inspect: { max_drill_down_items: 20 },
    };
    const config = loadWithUserConfig(user);
    expect(config.disabled_tools).toEqual(["glob"]);
    expect(config.indexes).toEqual({ trigram: true, semantic: false, callgraph: false });
    expect(config.github?.shim).toBe(false);
    // The extension never writes the user file for retired keys: the AFT
    // binary rewrites it on configure and reports that itself, so the
    // extension queues no retired-key notice of its own.
    const userPath = join(root, "xdg", "cortexkit", "aft.jsonc");
    expect(readFileSync(userPath, "utf8")).toBe(JSON.stringify(user));
    expect(
      getConfigLoadNotices().filter(
        (notice) => notice.configPath === userPath && notice.message.includes("retired keys"),
      ),
    ).toEqual([]);
  });

  test("project retired names translate but cannot loosen LSP idle limits or rewrite the file", () => {
    const projectFile = join(root, "project", ".cortexkit", "aft.jsonc");
    mkdirSync(join(root, "project", ".cortexkit"), { recursive: true });
    const text = JSON.stringify({
      search_index: false,
      idle: { lsp_ttl_minutes: 120 },
    });
    writeFileSync(projectFile, text);
    const config = loadWithUserConfig({ lsp: { idle_minutes: 30 } });
    expect(config.indexes).toEqual({ trigram: false, semantic: true, callgraph: true });
    expect(config.lsp?.idle_minutes).toBe(30);
    expect(config.disabled_tools).toEqual(["aft_delete", "aft_move"]);
    expect(
      getConfigLoadNotices().some(
        (notice) => notice.configPath === projectFile && notice.message.includes("retired keys"),
      ),
    ).toBe(true);
    expect(readFileSync(projectFile, "utf8")).toBe(text);
  });

  test("an older user key still translates and idle.checkout_hours merges per key", () => {
    const projectFile = join(root, "project", ".cortexkit", "aft.jsonc");
    mkdirSync(join(root, "project", ".cortexkit"), { recursive: true });
    writeFileSync(projectFile, JSON.stringify({ idle: { root_ttl_minutes: 15 } }));
    const user = {
      idle: { root_ttl_minutes: 20 },
      views: { idle_checkout_hours: 9000 },
    };
    const config = loadWithUserConfig(user);
    expect(config.idle).toEqual({ root_ttl_minutes: 15, checkout_hours: 8760 });
    expect(config).not.toHaveProperty("views");
    const userPath = join(root, "xdg", "cortexkit", "aft.jsonc");
    const notices = getConfigLoadNotices().filter(
      (notice) => notice.configPath === userPath && notice.message !== SEMANTIC_COST_NOTICE,
    );
    expect(notices).toEqual([]);
  });
});

describe("semantic default-on cost notice", () => {
  const costNotices = () =>
    getConfigLoadNotices().filter((notice) => notice.message === SEMANTIC_COST_NOTICE);

  test("is queued when an existing config relies on the old default", () => {
    for (const user of [{}, { indexes: { trigram: false } }]) {
      loadWithUserConfig(user);
      expect(costNotices()).toHaveLength(1);
      expect(costNotices()[0]?.configPath).toBe(join(root, "xdg", "cortexkit", "aft.jsonc"));
    }
  });

  // A fresh install has no config that ever relied on the old default: either
  // there is no file yet, or setup just wrote one with explicit indexes.
  // Neither is a migration, so neither gets the migration notice.
  test("is not queued on a fresh install", () => {
    loadWithUserConfig(undefined);
    expect(costNotices()).toHaveLength(0);
    loadWithUserConfig({
      disabled_tools: ["aft_delete", "aft_move"],
      indexes: { trigram: true, semantic: true, callgraph: true },
      github: { write: false, read: false },
    });
    expect(costNotices()).toHaveLength(0);
  });

  test("is not queued when the user configured semantic or chose another backend", () => {
    for (const user of [
      { indexes: { semantic: true } },
      { indexes: { semantic: false } },
      { semantic_search: true },
      { experimental_semantic_search: true },
      { harnesses: { pi: { indexes: { semantic: true } } } },
      { semantic: { backend: "openai_compatible", base_url: "http://localhost:1" } },
    ]) {
      loadWithUserConfig(user);
      expect(costNotices()).toHaveLength(0);
    }
  });
});

describe("removed data-layer config keys", () => {
  const cases = [
    {
      key: "views.enabled",
      doc: { views: { enabled: true } },
      notice:
        "views.enabled is gone because AFT now keeps one index store per repository and always uses it.",
    },
    {
      key: "views.callgraph_copies",
      doc: { views: { callgraph_copies: 2 } },
      notice:
        "views.callgraph_copies is gone because no checkout borrows another checkout's call graph any more.",
    },
    {
      key: "worktree.ram_overlay",
      doc: { worktree: { ram_overlay: false } },
      notice:
        "worktree.ram_overlay is gone because every worktree now has its own index state, so there is no borrowed index to patch in memory.",
    },
  ];
  for (const { key, doc, notice } of cases) {
    for (const harness of [false, true]) {
      const input = harness ? { harnesses: { pi: doc } } : doc;
      const keyPath = harness ? `harnesses.pi.${key}` : key;
      test(`user config refuses ${keyPath}`, () => {
        expect(() => loadWithUserConfig(input)).toThrow(
          `removed_config_key:${keyPath}; run npx @cortexkit/aft doctor --fix`,
        );
        expect(readFileSync(join(root, "xdg", "cortexkit", "aft.jsonc"), "utf8")).toBe(
          JSON.stringify(input),
        );
      });
      test(`project config drops ${keyPath} with its existing notice`, () => {
        const projectFile = join(root, "project", ".cortexkit", "aft.jsonc");
        mkdirSync(join(root, "project", ".cortexkit"), { recursive: true });
        writeFileSync(projectFile, JSON.stringify(input));
        const config = loadWithUserConfig({ format_on_edit: true });
        expect(getConfigValidationErrors()).toEqual([]);
        expect(config.format_on_edit).toBe(true);
        expect(config).not.toHaveProperty("views");
        expect(config).not.toHaveProperty("worktree");
        expect(
          getConfigLoadNotices()
            .filter((item) => item.configPath === projectFile)
            .map((item) => item.message),
        ).toEqual([notice]);
        expect(readFileSync(projectFile, "utf8")).toBe(JSON.stringify(input));
      });
    }
  }
});
