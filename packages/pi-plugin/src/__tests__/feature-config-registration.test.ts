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
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { ADAPTER_UNIMPLEMENTED_TOOLS, CANONICAL_TOOLS } from "@cortexkit/aft-bridge";

import {
  type AftConfig,
  ConfigRejectedError,
  loadAftConfig,
  setFeatureConfigPolicyVersionForTests,
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
  setFeatureConfigPolicyVersionForTests(undefined);
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

  test("after the window retired keys and aliases reject the whole load", () => {
    setFeatureConfigPolicyVersionForTests("0.59.0");
    let rejected: unknown;
    try {
      loadWithUserConfig({ disabled_tools: ["aft_glob"] });
    } catch (err) {
      rejected = err;
    }
    expect(rejected).toBeInstanceOf(ConfigRejectedError);
    expect((rejected as ConfigRejectedError).errors).toEqual([
      "removed_config_key:aft_glob:use:glob",
    ]);
  });
});
