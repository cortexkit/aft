/// <reference path="../bun-test.d.ts" />

/**
 * Regression tests for the upgrade drill: a 0.57-era user with an old config
 * and data upgraded to 0.58, then ran `doctor`, `doctor --fix` and `setup`.
 * Everything runs under a temporary HOME/XDG/AFT_CACHE_DIR and cwd so nothing
 * touches the operator's real config, cache or project.
 */

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import type { HarnessAdapter, HarnessConfigPaths } from "../adapters/types.js";
import { runDoctor } from "../commands/doctor.js";
import { diagnoseOpenCodeLoad } from "../doctor/opencode.js";
import {
  collectDiagnosticIssues,
  collectDiagnostics,
  type DiagnosticReport,
  type HarnessDiagnostic,
} from "../lib/diagnostics.js";
import type { NativeResult } from "../lib/feature-plan.js";
import { AFT_SCHEMA_URL, ensureAftSchemaUrl } from "../lib/jsonc.js";
import { getSelfVersion } from "../lib/self-version.js";
import type { OpenCodeHostDetection } from "../setup/host-generation.js";
import {
  AFT_OPENCODE_PACKAGE,
  ensurePinnedPluginConfig,
  pinnedPluginEntry,
  pluginConfigNeedsUpdate,
} from "../setup/opencode-config.js";

const ENV_KEYS = [
  "HOME",
  "XDG_CONFIG_HOME",
  "XDG_CACHE_HOME",
  "XDG_DATA_HOME",
  "AFT_CACHE_DIR",
  "AFT_STORAGE_DIR",
] as const;
const savedEnv: Partial<Record<(typeof ENV_KEYS)[number], string | undefined>> = {};
const originalCwd = process.cwd();
const originalStdoutWrite = process.stdout.write;
const originalStderrWrite = process.stderr.write;
const originalLog = console.log;
let sandbox = "";
let userConfig = "";

beforeEach(() => {
  sandbox = mkdtempSync(join(tmpdir(), "aft-cli-upgrade-"));
  for (const key of ENV_KEYS) savedEnv[key] = process.env[key];
  process.env.HOME = join(sandbox, "home");
  process.env.XDG_CONFIG_HOME = join(sandbox, "config");
  process.env.XDG_CACHE_HOME = join(sandbox, "cache");
  process.env.XDG_DATA_HOME = join(sandbox, "data");
  process.env.AFT_CACHE_DIR = join(sandbox, "aft-cache");
  process.env.AFT_STORAGE_DIR = join(sandbox, "storage");
  mkdirSync(join(sandbox, "project"), { recursive: true });
  mkdirSync(process.env.HOME, { recursive: true });
  process.chdir(join(sandbox, "project"));
  userConfig = join(sandbox, "config", "cortexkit", "aft.jsonc");
  mkdirSync(join(sandbox, "config", "cortexkit"), { recursive: true });
});

afterEach(() => {
  process.chdir(originalCwd);
  for (const key of ENV_KEYS) {
    if (savedEnv[key] === undefined) delete process.env[key];
    else process.env[key] = savedEnv[key];
  }
  process.stdout.write = originalStdoutWrite;
  process.stderr.write = originalStderrWrite;
  console.log = originalLog;
});

function captureOutput(): string[] {
  const output: string[] = [];
  const capture = ((chunk: string | Uint8Array) => {
    output.push(String(chunk));
    return true;
  }) as typeof process.stdout.write;
  process.stdout.write = capture;
  process.stderr.write = capture as typeof process.stderr.write;
  console.log = (...args: unknown[]) => output.push(args.join(" "));
  return output;
}

function v1(): OpenCodeHostDetection {
  return {
    status: "v1",
    generations: ["v1"],
    evidence: [
      {
        generation: "v1",
        executable: "/fixture/opencode",
        version: "1.18.32",
        runtime: "node",
        modernV1: true,
      },
    ],
  };
}

function v2(): OpenCodeHostDetection {
  return {
    status: "v2",
    generations: ["v2"],
    evidence: [
      {
        generation: "v2",
        executable: "/fixture/opencode2",
        version: null,
        runtime: "node",
        modernV1: false,
      },
    ],
  };
}

/** A registered OpenCode harness whose AFT config is the sandbox user file. */
function fixtureAdapter(): HarnessAdapter {
  const root = join(sandbox, "opencode");
  mkdirSync(root, { recursive: true });
  const configPaths: HarnessConfigPaths = {
    configDir: root,
    harnessConfig: join(root, "opencode.json"),
    harnessConfigFormat: "json",
    aftConfig: userConfig,
    aftConfigFormat: "jsonc",
  };
  writeFileSync(
    configPaths.harnessConfig,
    JSON.stringify({ plugin: [pinnedPluginEntry(getSelfVersion())] }),
  );
  return {
    kind: "opencode",
    displayName: "OpenCode",
    pluginPackageName: AFT_OPENCODE_PACKAGE,
    pluginEntryWithVersion: pinnedPluginEntry(getSelfVersion()),
    isInstalled: () => true,
    getHostVersion: () => "1.18.32",
    detectConfigPaths: () => configPaths,
    hasPluginEntry: () => true,
    ensurePluginEntry: async () => ({
      ok: true,
      action: "already_present",
      message: "already present",
      configPath: configPaths.harnessConfig,
    }),
    getPluginCacheInfo: () => ({ path: join(root, "cache"), exists: false }),
    getStorageDir: () => join(sandbox, "storage"),
    getLogFile: () => join(root, "aft-plugin.log"),
    getInstallHint: () => "fixture",
    clearPluginCache: async () => ({ action: "not_found", path: join(root, "cache") }),
  };
}

function writeUserConfig(value: unknown): void {
  writeFileSync(userConfig, `${JSON.stringify(value, null, 2)}\n`);
}

async function harnessFor(config: unknown): Promise<HarnessDiagnostic> {
  writeUserConfig(config);
  const report = await collectDiagnostics([fixtureAdapter()]);
  return report.harnesses[0] as HarnessDiagnostic;
}

async function plainDoctor(): Promise<{ code: number; text: string }> {
  const adapter = fixtureAdapter();
  const output = captureOutput();
  const code = await runDoctor({
    clear: false,
    fix: false,
    force: false,
    issue: false,
    argv: [],
    resolveAdapters: async () => [adapter],
    collectRemovalHealth: async () => ({ available: false, message: "fixture" }),
    detectOpenCodeHost: v1,
    runNative: () => ({
      ok: true,
      stdout: '{"plan_version":1,"features":[]}',
      stderr: "",
      status: 0,
    }),
  });
  return { code, text: output.join("") };
}

describe("doctor reports every condition that stops the plugin loading", () => {
  test("a subc.connection_file that points at nothing is HIGH, with what to remove", async () => {
    writeUserConfig({ subc: { connection_file: "~/.local/share/cortexkit/run/subc.json" } });
    const { code, text } = await plainDoctor();
    expect(code).toBe(1);
    expect(text).toContain("plugin registered: yes, but it will not load");
    expect(text).toContain("[HIGH] OpenCode: The plugin refuses to start: subc.connection_file");
    expect(text).toContain(
      `remove the "subc" block (or its "connection_file" key) from ${userConfig}`,
    );
    expect(text).toContain("doctor --fix does not edit this setting");
  });

  test("an existing connection file is not reported", async () => {
    const file = join(sandbox, "home", "subc.json");
    writeFileSync(file, "{}");
    const harness = await harnessFor({ subc: { connection_file: file } });
    expect(harness.pluginLoad?.blockers).toEqual([]);
  });

  test("rejected retired keys are HIGH and planned for doctor --fix", async () => {
    // gh_read is rejected at every version, so the plugin refuses the file.
    writeUserConfig({ gh_read: true });
    const { code, text } = await plainDoctor();
    expect(code).toBe(1);
    expect(text).toContain("[HIGH] OpenCode: The plugin refuses to start with");
    expect(text).toContain("gh_read → github.read");
    expect(text).toContain("doctor --fix` to migrate the file");
    const { buildDoctorFixPlan } = await import("../commands/doctor.js");
    const plan = buildDoctorFixPlan(
      [fixtureAdapter()],
      fixReport(fixtureAdapter(), getSelfVersion()),
    );
    expect(plan.find((item) => item.kind === "config")?.message).toContain(
      "replaced gh_read with github.read",
    );
  });

  test("a config that does not parse is HIGH with its path", async () => {
    writeFileSync(userConfig, "{ not json");
    const report = await collectDiagnostics([fixtureAdapter()]);
    const parse = collectDiagnosticIssues(report).filter(
      (issue) => issue.code === "config_parse_error",
    );
    expect(parse).toHaveLength(1);
    expect(parse[0]?.severity).toBe("high");
    expect(parse[0]?.message).toContain(userConfig);
    expect(parse[0]?.remediation).toContain(`Fix the JSON/JSONC syntax in ${userConfig}`);
    // The plugin still starts on defaults, so doctor must not claim it will not load.
    expect(parse[0]?.message).toContain("runs on defaults");
    const { text } = await plainDoctor();
    expect(text).toContain("[HIGH] OpenCode: AFT config");
    expect(text).not.toContain("it will not load");
  });
});

/** A diagnostics report for doctor --fix with a matching binary, so only config work remains. */
function fixReport(adapter: HarnessAdapter, binaryVersion: string | null): DiagnosticReport {
  const configPaths = adapter.detectConfigPaths();
  return {
    timestamp: new Date(0).toISOString(),
    platform: process.platform,
    arch: process.arch,
    nodeVersion: process.version,
    cliVersion: getSelfVersion(),
    binaryVersion,
    harnesses: [
      {
        kind: "opencode",
        displayName: "OpenCode",
        hostInstalled: true,
        hostVersion: "1.18.32",
        pluginRegistered: true,
        configPaths,
        aftConfig: { exists: true, enabled: true, flags: {} },
        pluginCache: { path: join(sandbox, "cache"), exists: false },
        storageDir: {
          path: join(sandbox, "storage"),
          exists: true,
          accessible: true,
          sizesByKey: {},
        },
        onnxRuntime: {
          required: false,
          systemPath: null,
          systemVersion: null,
          systemCompatible: null,
          cachedPath: null,
          cachedVersion: null,
          cachedCompatible: null,
          platform: "fixture",
          installHint: "fixture",
          autoDownloadable: true,
          requirement: "fixture",
        },
        logFile: { path: join(sandbox, "aft-plugin.log"), exists: false, sizeKb: 0 },
      },
    ],
    binaryCache: { path: join(sandbox, "aft-cache"), versions: [], totalSize: 0 },
    lspCache: {
      npm: { path: join(sandbox, "npm"), entries: [], totalSize: 0 },
      github: { path: join(sandbox, "gh"), entries: [], totalSize: 0 },
      totalSize: 0,
    },
  };
}

async function doctorFix(
  binaryVersion: string | null,
  runNative: (args: string[]) => NativeResult,
  downloadBinary?: (tag: string) => Promise<string | null>,
): Promise<string> {
  const adapter = fixtureAdapter();
  mkdirSync(join(sandbox, "storage"), { recursive: true });
  const output = captureOutput();
  await runDoctor({
    clear: false,
    fix: true,
    force: false,
    issue: false,
    argv: ["--fix", "--yes"],
    resolveAdapters: async () => [adapter],
    collectDiagnostics: async () => fixReport(adapter, binaryVersion),
    detectOpenCodeHost: v1,
    runNative,
    ...(downloadBinary ? { downloadBinary } : {}),
  });
  return output.join("");
}

describe("the config migration is planned and its changes are reported", () => {
  test("lists the migration with its changes, then prints what changed", async () => {
    writeUserConfig({
      $schema: AFT_SCHEMA_URL,
      semantic_search: false,
      search_index: true,
    });
    const migrated = {
      $schema: AFT_SCHEMA_URL,
      indexes: { semantic: false, trigram: true },
    };
    const text = await doctorFix(getSelfVersion(), (args) => {
      if (args[0] !== "fix-config") return { ok: false, stdout: "", stderr: "", status: 1 };
      writeUserConfig(migrated);
      return {
        ok: true,
        stdout: JSON.stringify({
          files: [{ path: userConfig, tier: "user", status: "rewritten", notes: [], error: null }],
        }),
        stderr: "",
        status: 0,
      };
    });
    const plan = text.slice(text.indexOf("Planned changes"), text.indexOf("Migrated"));
    expect(plan).toContain(`Will migrate retired keys in the user config ${userConfig}:`);
    expect(plan).toContain("removed semantic_search (was false)");
    expect(plan).toContain("added indexes.semantic: false");
    const report = text.slice(text.indexOf("Migrated user config"));
    expect(report).toContain("removed search_index (was true)");
    expect(report).toContain("removed semantic_search (was false)");
    expect(report).toContain("added indexes.trigram: true");
    expect(report).toContain("added indexes.semantic: false");
  });

  test("a config with nothing retired plans no migration", async () => {
    writeUserConfig({ $schema: AFT_SCHEMA_URL, indexes: { semantic: false } });
    const text = await doctorFix(getSelfVersion(), () => ({
      ok: true,
      stdout: JSON.stringify({
        files: [{ path: userConfig, tier: "user", status: "unchanged", notes: [], error: null }],
      }),
      stderr: "",
      status: 0,
    }));
    expect(text).not.toContain("Will migrate retired keys");
    expect(text).not.toContain("Migrated");
  });
});

describe("a binary already in the versioned cache", () => {
  test("is planned as a check, not a download, and never reported as not found", async () => {
    writeUserConfig({ $schema: AFT_SCHEMA_URL });
    const tag = `v${getSelfVersion()}`;
    const cached = join(process.env.AFT_CACHE_DIR as string, "bin", tag, "aft");
    mkdirSync(join(process.env.AFT_CACHE_DIR as string, "bin", tag), { recursive: true });
    writeFileSync(cached, "#!/bin/sh\n", { mode: 0o755 });
    const text = await doctorFix(
      null,
      () => ({ ok: true, stdout: '{"files":[]}', stderr: "", status: 0 }),
      async () => cached,
    );
    expect(text).toContain(`Will check the cached aft binary at ${cached}`);
    expect(text).not.toContain("Will download");
    expect(text).not.toMatch(/not found/i);
    expect(text).toContain(`The cached AFT binary at ${cached} is ${tag}; nothing to download.`);
  });

  test("a cached binary the downloader replaced is reported as installed, not reused", async () => {
    writeUserConfig({ $schema: AFT_SCHEMA_URL });
    const tag = `v${getSelfVersion()}`;
    const dir = join(process.env.AFT_CACHE_DIR as string, "bin", tag);
    const cached = join(dir, "aft");
    mkdirSync(dir, { recursive: true });
    writeFileSync(cached, "#!/bin/sh\n", { mode: 0o755 });
    const text = await doctorFix(
      null,
      () => ({ ok: true, stdout: '{"files":[]}', stderr: "", status: 0 }),
      async () => {
        writeFileSync(cached, "#!/bin/sh\necho replaced\n", { mode: 0o755 });
        return cached;
      },
    );
    expect(text).toContain(`AFT binary installed at ${cached}`);
    expect(text).not.toContain("nothing to download");
  });

  test("with no cached binary the plan says it will download, and where", async () => {
    writeUserConfig({ $schema: AFT_SCHEMA_URL });
    const text = await doctorFix(
      null,
      () => ({ ok: true, stdout: '{"files":[]}', stderr: "", status: 0 }),
      async () => join(sandbox, "fresh", "aft"),
    );
    expect(text).toContain(
      `Will download the aft binary v${getSelfVersion()} into ${join(process.env.AFT_CACHE_DIR as string, "bin")}`,
    );
  });
});

describe("ONNX Runtime is required only for the local semantic backend", () => {
  test("a remote embedding backend does not need ONNX, even with the old semantic_search key", async () => {
    const harness = await harnessFor({
      semantic_search: true,
      semantic: { backend: "openai_compatible", base_url: "https://example.invalid" },
    });
    expect(harness.onnxRuntime.required).toBe(false);
  });

  test("the default (semantic on, local backend) does need ONNX", async () => {
    const harness = await harnessFor({});
    expect(harness.onnxRuntime.required).toBe(true);
  });

  test("the semantic index switched off does not need ONNX", async () => {
    const harness = await harnessFor({ indexes: { semantic: false } });
    expect(harness.onnxRuntime.required).toBe(false);
  });
});

describe("the pin message and doctor --fix agree", () => {
  const pinned = pinnedPluginEntry(getSelfVersion());

  function problemsFor(entry: string, detection: OpenCodeHostDetection): string[] {
    const root = join(sandbox, "oc");
    mkdirSync(root, { recursive: true });
    const key = detection.status === "v2" ? "plugins" : "plugin";
    writeFileSync(join(root, "opencode.json"), JSON.stringify({ [key]: [entry] }));
    return diagnoseOpenCodeLoad({
      detection,
      configPath: join(root, "opencode.json"),
      logPath: join(root, "none.log"),
      pluginCachePath: join(root, "cache"),
      expectedPluginEntry: pinned,
      acceptExplicitPluginVersion: detection.status === "v1",
    }).problems;
  }

  test("an unversioned V1 entry names the exact version --fix writes", () => {
    const problems = problemsFor(AFT_OPENCODE_PACKAGE, v1());
    expect(problems).toHaveLength(1);
    expect(problems[0]).toContain(`pin it to ${pinned}`);
    expect(problems[0]).toContain("has no version");
    expect(problems[0]).not.toContain("@latest");
    const config: Record<string, unknown> = { plugin: [AFT_OPENCODE_PACKAGE] };
    ensurePinnedPluginConfig(config, getSelfVersion(), () => false, "v1");
    expect(config.plugin).toEqual([pinned]);
  });

  test("on V1, @latest and a chosen version load fine, so doctor does not report them", () => {
    for (const entry of [`${AFT_OPENCODE_PACKAGE}@latest`, `${AFT_OPENCODE_PACKAGE}@0.55.0`]) {
      expect(problemsFor(entry, v1())).toEqual([]);
    }
  });

  test("on V2 any other version is reported and --fix rewrites it to the same entry", () => {
    const entry = `${AFT_OPENCODE_PACKAGE}@latest`;
    const problems = problemsFor(entry, v2());
    expect(problems[0]).toContain(`pin it to ${pinned}`);
    expect(pluginConfigNeedsUpdate({ plugins: [entry] }, getSelfVersion(), () => false, "v2")).toBe(
      true,
    );
    const config: Record<string, unknown> = { plugins: [entry] };
    ensurePinnedPluginConfig(config, getSelfVersion(), () => false, "v2");
    expect(config.plugins).toEqual([pinned]);
  });
});

describe("$schema goes first in an existing aft.jsonc", () => {
  test("inserted as the first key, with the user's comments and keys kept", () => {
    const text = [
      "// my AFT settings",
      "{",
      "  // remote embeddings",
      '  "semantic": { "backend": "openai_compatible" },',
      '  "indexes": { "trigram": true } // keep grep fast',
      "}",
      "",
    ].join("\n");
    writeFileSync(userConfig, text);
    expect(ensureAftSchemaUrl(userConfig, "jsonc").action).toBe("added");
    const written = readFileSync(userConfig, "utf8");
    const lines = written.split("\n");
    expect(lines[0]).toBe("// my AFT settings");
    expect(lines[1]).toBe("{");
    expect(lines[2]).toBe(`  "$schema": "${AFT_SCHEMA_URL}",`);
    expect(written).toContain("// remote embeddings");
    expect(written).toContain("// keep grep fast");
    expect(written).toContain('"backend": "openai_compatible"');
  });

  test("an outdated $schema elsewhere in the file moves to the top", () => {
    writeFileSync(userConfig, '{\n  "indexes": { "trigram": true },\n  "$schema": "old"\n}\n');
    ensureAftSchemaUrl(userConfig, "jsonc");
    const parsed = Object.keys(JSON.parse(readFileSync(userConfig, "utf8")));
    expect(parsed).toEqual(["$schema", "indexes"]);
    expect(existsSync(userConfig)).toBe(true);
  });
});
