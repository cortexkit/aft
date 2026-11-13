/// <reference path="../bun-test.d.ts" />

/**
 * Feature-config registration policy on the OpenCode V1 and V2 adapters.
 *
 * Every case loads real config files through `loadAftConfig`, so the legacy
 * translation, the absent-base default and the explicit-list presence rules
 * are exercised end to end. A tool registers exactly when its canonical name
 * is absent from the resolved `disabled_tools`.
 */

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  CANONICAL_TOOLS,
  REMOVED_DATA_LAYER_NOTES,
  SEMANTIC_COST_NOTICE,
} from "@cortexkit/aft-bridge";

import {
  type AftConfig,
  getConfigLoadNotices,
  getConfigValidationErrors,
  loadAftConfig,
} from "../config.js";
import { buildOpenCodeToolMap, registerAftTools } from "../tool-registration.js";
import type { V2ProviderTool } from "../tools/definitions/v2.js";
import type { PluginContext } from "../types.js";
import { v2HostToolName } from "../v2-host-tool-names.js";

const HOSTS = ["apply_patch", "bash", "edit", "glob", "grep", "read", "write"];
const CODEMODE_NAMES: Readonly<Record<string, string>> = {
  read: "read",
  grep: "grep",
  glob: "glob",
  aft_search: "search",
  aft_outline: "outline",
  aft_zoom: "zoom",
  aft_callgraph: "callgraph",
};

/** Expected registered names for each user config document (undefined = no file). */
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
let savedEnv: Record<string, string | undefined>;

beforeEach(() => {
  root = mkdtempSync(join(tmpdir(), "aft-oc-feature-registration-"));
  savedEnv = {
    HOME: process.env.HOME,
    XDG_CONFIG_HOME: process.env.XDG_CONFIG_HOME,
    OPENCODE_CONFIG_DIR: process.env.OPENCODE_CONFIG_DIR,
  };
  process.env.HOME = join(root, "home");
  process.env.XDG_CONFIG_HOME = join(root, "xdg");
  delete process.env.OPENCODE_CONFIG_DIR;
});

afterEach(() => {
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

function stubContext(config: AftConfig): PluginContext {
  const pool = {
    getBridge: () => {
      throw new Error("registration must not touch the bridge");
    },
  };
  return { pool, config, storageDir: join(root, "storage") } as never;
}

function v1Names(config: AftConfig): string[] {
  return Object.keys(buildOpenCodeToolMap(stubContext(config), config)).sort();
}

type V2Registration = [namespace: unknown, name: string, codemode: boolean];

function v2Registrations(config: AftConfig): V2Registration[] {
  const definitions = buildOpenCodeToolMap(stubContext(config), config);
  const added: V2Registration[] = [];
  registerAftTools(
    {
      tool: {
        transform(register) {
          register({
            add: (definition: V2ProviderTool) =>
              added.push([
                definition.options.namespace,
                definition.name,
                definition.options.codemode,
              ]),
            remove: () => {},
          });
        },
      },
    },
    { directory: join(root, "project") },
    definitions,
  );
  return added.sort();
}

function expected(disabled: readonly string[]): string[] {
  return CANONICAL_TOOLS.filter((tool) => !disabled.includes(tool)).sort();
}

function expectedV2(disabled: readonly string[]): V2Registration[] {
  return expected(disabled)
    .flatMap<V2Registration>((toolName) => {
      const direct: V2Registration = [undefined, v2HostToolName(toolName), false];
      const codeModeName = CODEMODE_NAMES[toolName];
      return codeModeName ? [direct, ["aft.code", codeModeName, true]] : [direct];
    })
    .sort();
}

describe("OpenCode feature-config registration", () => {
  for (const { name, user, disabled } of CASES) {
    test(`V1 registers canonical tools minus resolved disables: ${name}`, () => {
      const config = loadWithUserConfig(user);
      expect(config.disabled_tools).toEqual([...disabled].sort());
      expect(v1Names(config)).toEqual(expected(disabled));
    });

    test(`V2 registers canonical tools minus resolved disables: ${name}`, () => {
      // Disabling a direct tool must also remove its namespaced Code Mode copy.
      expect(v2Registrations(loadWithUserConfig(user))).toEqual(expectedV2(disabled));
    });
  }

  test("unknown disabled names are reported once, sorted, and stay inert", () => {
    const config = loadWithUserConfig({
      disabled_tools: ["typo_name", "aft_future_tool", "typo_name"],
    });
    const reports: Array<readonly string[]> = [];
    const tools = buildOpenCodeToolMap(stubContext(config), config, (unknown) =>
      reports.push(unknown),
    );
    expect(reports).toEqual([["aft_future_tool", "typo_name"]]);
    expect(Object.keys(tools).sort()).toEqual(expected([]));
    expect(config.disabled_tools).toEqual(["aft_future_tool", "typo_name"]);
  });

  test("legacy aliases canonicalize without an unknown-name report", () => {
    const config = loadWithUserConfig({ disabled_tools: ["aft_glob"] });
    const reports: Array<readonly string[]> = [];
    const tools = buildOpenCodeToolMap(stubContext(config), config, (unknown) =>
      reports.push(unknown),
    );
    expect(config.disabled_tools).toEqual(["glob"]);
    expect(reports).toEqual([]);
    expect(Object.keys(tools)).not.toContain("glob");
    expect(Object.keys(tools)).not.toContain("aft_glob");
  });

  test("retired keys in the user file translate and are left to the engine to rewrite", () => {
    const user = {
      disabled_tools: ["aft_glob"],
      tool_surface: "all",
      hoist_builtin_tools: false,
      enabled: false,
      search_index: true,
      experimental_search_index: false,
      semantic_search: false,
      experimental_semantic_search: true,
      callgraph_store: false,
      gh_read: { enabled: true },
      idle: { lsp_ttl_minutes: 10 },
    };
    const config = loadWithUserConfig(user);
    expect(config.disabled_tools).toEqual(["glob"]);
    expect(config.indexes).toEqual({ trigram: true, semantic: false, callgraph: false });
    expect(config.github?.read).toBe(true);
    expect(config.lsp?.idle_minutes).toBe(10);
    // The plugin never writes the user file for retired keys: the AFT binary
    // rewrites it on configure and reports that itself, so the plugin queues
    // no retired-key notice of its own.
    const userPath = join(root, "xdg", "cortexkit", "aft.jsonc");
    expect(readFileSync(userPath, "utf8")).toBe(JSON.stringify(user));
    expect(
      getConfigLoadNotices().filter(
        (notice) => notice.configPath === userPath && notice.message.includes("retired keys"),
      ),
    ).toEqual([]);
  });

  test("project retired names translate within project limits and leave the file unwritten", () => {
    const projectFile = join(root, "project", ".cortexkit", "aft.jsonc");
    mkdirSync(join(root, "project", ".cortexkit"), { recursive: true });
    const text = JSON.stringify({
      search_index: false,
      hoist_builtin_tools: false,
      gh_read: { enabled: true },
      disabled_tools: ["aft_grep"],
    });
    writeFileSync(projectFile, text);
    const config = loadWithUserConfig({ github: { read: false } });
    expect(config.indexes).toEqual({ trigram: false, semantic: true, callgraph: true });
    expect(config.github?.read).toBe(false);
    // A project may not unregister the host's tools, so the translated
    // aft_grep name is ignored and only the user defaults remain disabled.
    expect(config.disabled_tools).toEqual(["aft_delete", "aft_move"]);
    expect(v1Names(config)).toEqual(expected(["aft_delete", "aft_move"]));
    expect(v2Registrations(config)).toEqual(expectedV2(["aft_delete", "aft_move"]));
    expect(
      getConfigLoadNotices().some(
        (notice) => notice.configPath === projectFile && notice.message.includes("retired keys"),
      ),
    ).toBe(true);
    expect(readFileSync(projectFile, "utf8")).toBe(text);
  });

  test("a user file naming retired keys still loads translated, because the binary rewrites it", () => {
    const config = loadWithUserConfig({ search_index: false, disabled_tools: ["aft_grep"] });
    expect(config.indexes?.trigram).toBe(false);
    expect(config.disabled_tools).toEqual(["grep"]);
  });

  test("a retired bridge.hang_threshold loads cleanly; a project file gets a notice that it does nothing", () => {
    const projectFile = join(root, "project", ".cortexkit", "aft.jsonc");
    mkdirSync(join(root, "project", ".cortexkit"), { recursive: true });
    writeFileSync(projectFile, JSON.stringify({ bridge: { hang_threshold: 4 } }));
    const config = loadWithUserConfig({
      bridge: { request_timeout_ms: 45_000, hang_threshold: 3 },
    });
    expect(config.bridge).toEqual({ request_timeout_ms: 45_000 });
    expect(getConfigValidationErrors()).toEqual([]);
    const notices = getConfigLoadNotices().filter((notice) => notice.configPath === projectFile);
    // Nothing was applied, so the notice is only the no-effect sentence.
    expect(notices.map((notice) => notice.message)).toEqual([
      "bridge.hang_threshold no longer does anything: a timed-out request no longer restarts AFT, so the setting can be removed.",
    ]);
  });

  test("a false runtime gate only switches its behaviour off", () => {
    expect(loadWithUserConfig({ backup: { enabled: false } }).disabled_tools).toEqual([
      "aft_delete",
      "aft_move",
    ]);
  });

  test("idle.checkout_hours loads, clamps and merges per key with the project file", () => {
    const projectFile = join(root, "project", ".cortexkit", "aft.jsonc");
    mkdirSync(join(root, "project", ".cortexkit"), { recursive: true });
    writeFileSync(projectFile, JSON.stringify({ idle: { root_ttl_minutes: 15 } }));
    const config = loadWithUserConfig({ idle: { root_ttl_minutes: 20, checkout_hours: 9000 } });
    expect(getConfigValidationErrors()).toEqual([]);
    expect(config.idle).toEqual({ root_ttl_minutes: 15, checkout_hours: 8760 });
    // The project file still sets root_ttl_minutes; the user's 0 clamps to 1.
    expect(loadWithUserConfig({ idle: { checkout_hours: 0 } }).idle).toEqual({
      root_ttl_minutes: 15,
      checkout_hours: 1,
    });
  });

  test("a user file's views.idle_checkout_hours becomes idle.checkout_hours; its removed keys get one notice naming only them", () => {
    const user = {
      views: { idle_checkout_hours: 72, enabled: true },
      worktree: { ram_overlay: false },
    };
    const config = loadWithUserConfig(user);
    expect(getConfigValidationErrors()).toEqual([]);
    expect(config.idle).toEqual({ checkout_hours: 72 });
    // The removed keys still reach the engine until the code reading them is deleted.
    expect(config.views).toEqual({ enabled: true });
    expect(config.worktree).toEqual({ ram_overlay: false });
    const userPath = join(root, "xdg", "cortexkit", "aft.jsonc");
    expect(readFileSync(userPath, "utf8")).toBe(JSON.stringify(user));
    const notices = getConfigLoadNotices().filter(
      (notice) => notice.configPath === userPath && notice.message !== SEMANTIC_COST_NOTICE,
    );
    expect(notices.map((notice) => notice.message)).toEqual([
      `${userPath} sets removed keys (views.enabled, worktree.ram_overlay); AFT ignores them, and from 0.65.0 it refuses a config file that sets them. Run \`npx @cortexkit/aft doctor --fix\` to remove them. ${REMOVED_DATA_LAYER_NOTES[1]?.[1]} ${REMOVED_DATA_LAYER_NOTES[2]?.[1]}`,
    ]);
  });

  test("a project file with data-layer keys gets one notice covering the renamed and the removed keys", () => {
    const projectFile = join(root, "project", ".cortexkit", "aft.jsonc");
    mkdirSync(join(root, "project", ".cortexkit"), { recursive: true });
    writeFileSync(
      projectFile,
      JSON.stringify({ views: { idle_checkout_hours: 24, callgraph_copies: 0 } }),
    );
    const config = loadWithUserConfig({ idle: { checkout_hours: 2 } });
    expect(getConfigValidationErrors()).toEqual([]);
    expect(config.idle?.checkout_hours).toBe(24);
    const notices = getConfigLoadNotices().filter((notice) => notice.configPath === projectFile);
    expect(notices.map((notice) => notice.message)).toEqual([
      `${projectFile} uses retired keys (views.idle_checkout_hours); AFT applied their current equivalents, with the same limits a project config has for those keys. Run \`npx @cortexkit/aft doctor --fix\` to update the file. views.idle_checkout_hours is now idle.checkout_hours; from 0.65.0 AFT refuses a config file that still sets views.idle_checkout_hours. ${projectFile} sets removed keys (views.callgraph_copies); AFT ignores them, and from 0.65.0 it refuses a config file that sets them. Run \`npx @cortexkit/aft doctor --fix\` to remove them. ${REMOVED_DATA_LAYER_NOTES[0]?.[1]}`,
    ]);
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
      { harnesses: { opencode: { indexes: { semantic: true } } } },
      { semantic: { backend: "openai_compatible", base_url: "http://localhost:1" } },
    ]) {
      loadWithUserConfig(user);
      expect(costNotices()).toHaveLength(0);
    }
  });
});
