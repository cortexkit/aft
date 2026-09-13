/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, join } from "node:path";

import { acquireEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import { OmpAdapter } from "../adapters/omp.js";
import { collectDiagnostics } from "../lib/diagnostics.js";
import { OMP_PLUGIN_PACKAGE } from "../lib/omp-helpers.js";
import { getOmpPluginsLockPath } from "../lib/omp-paths.js";

type FakePlugin = { name: string; version: string; enabled: boolean };
type FakeState = { plugins: FakePlugin[]; projectOverrideDisabled?: boolean };

let root: string;
let statePath: string;
let argvPath: string;
let releaseEnv: (() => void) | undefined;
const originalPath = process.env.PATH ?? "";

function writeState(state: FakeState): void {
  writeFileSync(statePath, JSON.stringify(state));
}

function readState(): FakeState {
  return JSON.parse(readFileSync(statePath, "utf8")) as FakeState;
}

function commands(): string[][] {
  const raw = readFileSync(argvPath, "utf8").trim();
  return raw ? raw.split("\n").map((line) => JSON.parse(line) as string[]) : [];
}

function createFakeOmp(binDir: string): void {
  mkdirSync(binDir, { recursive: true });
  const scriptPath = join(binDir, "fake-omp.js");
  writeFileSync(
    scriptPath,
    `import { appendFileSync, readFileSync, writeFileSync } from "node:fs";
const argv = process.argv.slice(2);
appendFileSync(process.env.OMP_FAKE_ARGV, JSON.stringify(argv) + "\\n");
const statePath = process.env.OMP_FAKE_STATE;
const state = JSON.parse(readFileSync(statePath, "utf8"));
const save = () => writeFileSync(statePath, JSON.stringify(state));
if (argv[0] === "--version") {
  console.log("omp/17.1.7");
  process.exit(0);
}
if (argv.join(" ") === "plugin list --json") {
  const npm = state.plugins.map((plugin) => ({
    ...plugin,
    enabled: state.projectOverrideDisabled ? false : plugin.enabled,
  }));
  console.log(JSON.stringify({ npm }));
  process.exit(0);
}
const action = argv[1];
const name = argv[2];
if (argv[0] === "plugin" && action === "install") {
  if (!state.plugins.some((plugin) => plugin.name === name)) {
    state.plugins.push({ name, version: "0.55.1", enabled: true });
  }
  save();
  process.exit(0);
}
const plugin = state.plugins.find((entry) => entry.name === name);
if (argv[0] === "plugin" && action === "enable" && plugin) {
  plugin.enabled = true;
  save();
  process.exit(0);
}
if (argv[0] === "plugin" && action === "disable" && plugin) {
  plugin.enabled = false;
  save();
  process.exit(0);
}
if (argv[0] === "plugin" && action === "uninstall") {
  state.plugins = state.plugins.filter((entry) => entry.name !== name);
  save();
  process.exit(0);
}
console.error("unexpected fake OMP argv: " + argv.join(" "));
process.exit(2);
`,
  );

  if (process.platform === "win32") {
    writeFileSync(join(binDir, "omp.cmd"), `@echo off\r\nbun "${scriptPath}" %*\r\n`);
  } else {
    const wrapper = join(binDir, "omp");
    writeFileSync(wrapper, `#!/bin/sh\nexec bun "${scriptPath}" "$@"\n`);
    chmodSync(wrapper, 0o755);
  }
}

beforeEach(async () => {
  root = mkdtempSync(join(tmpdir(), "aft-cli-omp-adapter-"));
  statePath = join(root, "state.json");
  argvPath = join(root, "argv.jsonl");
  const binDir = join(root, "bin");
  createFakeOmp(binDir);
  writeState({ plugins: [] });
  writeFileSync(argvPath, "");

  releaseEnv = await acquireEnv({
    HOME: join(root, "home"),
    USERPROFILE: join(root, "home"),
    XDG_CONFIG_HOME: undefined,
    XDG_DATA_HOME: undefined,
    AFT_STORAGE_DIR: join(root, "aft-storage"),
    PI_CONFIG_DIR: undefined,
    PI_CODING_AGENT_DIR: undefined,
    PI_PACKAGE_DIR: undefined,
    PI_PROFILE: undefined,
    PI_CONFIG_FILES: undefined,
    OMP_PROFILE: undefined,
    OMP_FAKE_STATE: statePath,
    OMP_FAKE_ARGV: argvPath,
    PATH: `${binDir}${delimiter}${originalPath}`,
  });
});

afterEach(() => {
  releaseEnv?.();
  releaseEnv = undefined;
  rmSync(root, { recursive: true, force: true });
});

describe("OmpAdapter host-managed registration", () => {
  test("installs and removes the Pi-compatible plugin through the fake OMP host", async () => {
    const adapter = new OmpAdapter();

    const installed = await adapter.ensurePluginEntry();
    expect(installed).toMatchObject({ ok: true, action: "added" });
    expect(adapter.hasPluginEntry()).toBe(true);
    expect(adapter.getInstalledPluginVersion()).toBe("0.55.1");

    const removed = await adapter.removePluginEntry();
    expect(removed).toMatchObject({ ok: true, action: "updated" });
    expect(readState().plugins).toEqual([]);
    expect(commands()).toEqual([
      ["plugin", "list", "--json"],
      ["plugin", "install", OMP_PLUGIN_PACKAGE],
      ["plugin", "list", "--json"],
      ["plugin", "list", "--json"],
      ["plugin", "list", "--json"],
      ["plugin", "list", "--json"],
      ["plugin", "uninstall", OMP_PLUGIN_PACKAGE],
    ]);
  });

  test("rolls back a fresh install hidden by a project-level disable override", async () => {
    writeState({ plugins: [], projectOverrideDisabled: true });

    const result = await new OmpAdapter().ensurePluginEntry();

    expect(result).toMatchObject({ ok: false, action: "error" });
    expect(result.message).toContain("still disabled in the current project");
    expect(readState().plugins).toEqual([]);
    expect(commands()).toEqual([
      ["plugin", "list", "--json"],
      ["plugin", "install", OMP_PLUGIN_PACKAGE],
      ["plugin", "list", "--json"],
      ["plugin", "uninstall", OMP_PLUGIN_PACKAGE],
    ]);
  });

  test("restores the prior lockfile enable state after an overridden enable", async () => {
    writeState({
      plugins: [{ name: OMP_PLUGIN_PACKAGE, version: "0.55.1", enabled: false }],
      projectOverrideDisabled: true,
    });
    const lockPath = getOmpPluginsLockPath();
    mkdirSync(join(lockPath, ".."), { recursive: true });
    writeFileSync(
      lockPath,
      JSON.stringify({ plugins: { [OMP_PLUGIN_PACKAGE]: { enabled: false } } }),
    );

    const result = await new OmpAdapter().ensurePluginEntry();

    expect(result.ok).toBe(false);
    expect(readState().plugins[0]?.enabled).toBe(false);
    expect(commands()).toEqual([
      ["plugin", "list", "--json"],
      ["plugin", "enable", OMP_PLUGIN_PACKAGE],
      ["plugin", "list", "--json"],
      ["plugin", "disable", OMP_PLUGIN_PACKAGE],
    ]);
  });

  test("feeds OMP host, plugin version, and config paths into doctor diagnostics", async () => {
    writeState({
      plugins: [{ name: OMP_PLUGIN_PACKAGE, version: "0.55.1", enabled: true }],
    });
    const adapter = new OmpAdapter();

    const report = await collectDiagnostics([adapter]);
    const harness = report.harnesses[0];

    expect(harness).toMatchObject({
      kind: "omp",
      displayName: "Oh My Pi (OMP)",
      hostInstalled: true,
      hostVersion: "17.1.7",
      pluginRegistered: true,
      pluginCache: { cached: "0.55.1", exists: true },
      configPaths: {
        configDir: join(process.env.HOME!, ".omp", "agent"),
        harnessConfig: join(process.env.HOME!, ".omp", "plugins", "omp-plugins.lock.json"),
        aftConfig: join(process.env.HOME!, ".config", "cortexkit", "aft.jsonc"),
      },
    });
  });
});
