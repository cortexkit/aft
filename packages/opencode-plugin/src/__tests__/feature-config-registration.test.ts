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
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { CANONICAL_TOOLS, SEMANTIC_COST_NOTICE } from "@cortexkit/aft-bridge";

import {
  type AftConfig,
  ConfigRejectedError,
  getConfigLoadNotices,
  loadAftConfig,
  setFeatureConfigPolicyVersionForTests,
} from "../config.js";
import { buildOpenCodeToolMap, registerAftTools } from "../tool-registration.js";
import type { V2ProviderTool } from "../tools/definitions/v2.js";
import type { PluginContext } from "../types.js";

const HOSTS = ["apply_patch", "bash", "edit", "glob", "grep", "read", "write"];

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
  setFeatureConfigPolicyVersionForTests(undefined);
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

function v2Names(config: AftConfig): string[] {
  const definitions = buildOpenCodeToolMap(stubContext(config), config);
  const added: string[] = [];
  registerAftTools(
    {
      tool: {
        transform(register) {
          register({
            add: (definition: V2ProviderTool) => added.push(definition.name),
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

describe("OpenCode feature-config registration", () => {
  for (const { name, user, disabled } of CASES) {
    test(`V1 registers canonical tools minus resolved disables: ${name}`, () => {
      const config = loadWithUserConfig(user);
      expect(config.disabled_tools).toEqual([...disabled].sort());
      expect(v1Names(config)).toEqual(expected(disabled));
    });

    test(`V2 registers canonical tools minus resolved disables: ${name}`, () => {
      expect(v2Names(loadWithUserConfig(user))).toEqual(expected(disabled));
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

  test("in-window legacy aliases canonicalize without an unknown-name report", () => {
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

  test("after the window retired keys and aliases reject the whole load", () => {
    setFeatureConfigPolicyVersionForTests("0.59.0");
    let rejected: unknown;
    try {
      loadWithUserConfig({ disabled_tools: ["aft_glob"], tool_surface: "all" });
    } catch (err) {
      rejected = err;
    }
    expect(rejected).toBeInstanceOf(ConfigRejectedError);
    expect((rejected as ConfigRejectedError).errors).toEqual([
      "removed_config_key:aft_glob:use:glob",
      "removed_config_key:tool_surface:use:disabled_tools",
    ]);
    // Retained runtime gates stay accepted after the window.
    expect(loadWithUserConfig({ backup: { enabled: false } }).disabled_tools).toEqual([
      "aft_delete",
      "aft_move",
    ]);
  });
});

describe("semantic default-on cost notice", () => {
  const costNotices = () =>
    getConfigLoadNotices().filter((notice) => notice.message === SEMANTIC_COST_NOTICE);

  test("is queued when semantic is on only by default", () => {
    for (const user of [undefined, {}, { indexes: { trigram: false } }]) {
      loadWithUserConfig(user);
      expect(costNotices()).toHaveLength(1);
      expect(costNotices()[0]?.configPath).toBe(join(root, "xdg", "cortexkit", "aft.jsonc"));
    }
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
