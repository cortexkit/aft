/// <reference path="../bun-test.d.ts" />

import { afterEach, describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

import { OpenCodeAdapter } from "../adapters/opencode.js";
import type { HarnessAdapter, HarnessConfigPaths, PluginEntryResult } from "../adapters/types.js";
import { runDoctor } from "../commands/doctor.js";
import { runSetup } from "../commands/setup.js";
import { diagnoseOpenCodeLoad, OPENCODE_LOAD_PATHS } from "../doctor/opencode.js";
import type { DiagnosticReport, HarnessDiagnostic } from "../lib/diagnostics.js";
import { getSelfVersion } from "../lib/self-version.js";
import {
  detectOpenCodeHostGeneration,
  type OpenCodeHostDetection,
  probeOpenCodeV1Version,
} from "../setup/host-generation.js";
import {
  AFT_OPENCODE_PACKAGE,
  ensurePinnedPluginConfig,
  MODERN_V1_VERSION,
  pinnedPluginEntry,
} from "../setup/opencode-config.js";

const originalLog = console.log;
const originalStdoutWrite = process.stdout.write;
const originalStderrWrite = process.stderr.write;

afterEach(() => {
  console.log = originalLog;
  process.stdout.write = originalStdoutWrite;
  process.stderr.write = originalStderrWrite;
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

function tempRoot(label: string): string {
  return mkdtempSync(join(tmpdir(), label));
}

function writeHostPackage(
  root: string,
  name: "opencode-ai" | "@opencode-ai/cli",
  executableName: "opencode" | "opencode2",
  version: string,
): string {
  const packageRoot = join(root, "node_modules", ...name.split("/"));
  const executable = join(packageRoot, "bin", executableName);
  mkdirSync(join(packageRoot, "bin"), { recursive: true });
  writeFileSync(executable, "host fixture\n", { mode: 0o755 });
  writeFileSync(join(packageRoot, "package.json"), JSON.stringify({ name, version }));
  return executable;
}

function detection(
  status: OpenCodeHostDetection["status"],
  runtime: "bun" | "node" = "bun",
): OpenCodeHostDetection {
  const generations =
    status === "v1" || status === "v2" ? [status] : status === "ambiguous" ? ["v1", "v2"] : [];
  return {
    status,
    generations,
    evidence: generations.map((generation) => ({
      generation,
      executable: generation === "v1" ? "/fixture/opencode" : "/fixture/opencode2",
      version: generation === "v1" ? MODERN_V1_VERSION : null,
      runtime,
      modernV1: generation === "v1",
    })),
  };
}

class ConfiguredOpenCodeAdapter extends OpenCodeAdapter {
  ensureCalls = 0;

  constructor(private readonly root: string) {
    super();
  }

  override isInstalled(): boolean {
    return true;
  }

  override detectConfigPaths(): HarnessConfigPaths {
    const harnessConfig = join(this.root, "opencode.json");
    const aftConfig = join(this.root, "aft.jsonc");
    const tuiConfig = join(this.root, "tui.json");
    return {
      configDir: this.root,
      harnessConfig,
      harnessConfigFormat: existsSync(harnessConfig) ? "json" : "none",
      aftConfig,
      aftConfigFormat: existsSync(aftConfig) ? "jsonc" : "none",
      tuiConfig,
      tuiConfigFormat: existsSync(tuiConfig) ? "json" : "none",
    };
  }

  override async ensurePluginEntry(): Promise<PluginEntryResult> {
    this.ensureCalls += 1;
    return super.ensurePluginEntry();
  }
}

describe("OpenCode generation detection", () => {
  test("reads the modern V1 pin from the repository source of truth", () => {
    const repositoryPin = readFileSync(
      new URL("../../../../.github/opencode-version.txt", import.meta.url),
      "utf8",
    ).trim();
    expect(MODERN_V1_VERSION).toBe(repositoryPin);
  });

  test("classifies package metadata and reports both generations when both are installed", () => {
    const root = tempRoot("aft-cli-host-metadata-");
    const v1 = writeHostPackage(root, "opencode-ai", "opencode", MODERN_V1_VERSION);
    const v2 = writeHostPackage(root, "@opencode-ai/cli", "opencode2", "0.0.0-beta-fixture");
    const probed: string[] = [];

    const result = detectOpenCodeHostGeneration({
      findExecutable: (name) => (name === "opencode" ? v1 : v2),
      probeV1Version: (executable) => {
        probed.push(executable);
        return null;
      },
    });

    expect(result.status).toBe("ambiguous");
    expect(result.generations).toEqual(["v1", "v2"]);
    expect(result.evidence.map((item) => item.version)).toEqual([
      MODERN_V1_VERSION,
      "0.0.0-beta-fixture",
    ]);
    expect(probed).toEqual([]);
  });

  test("never executes opencode2 when V1 needs a fallback version probe", () => {
    const root = tempRoot("aft-cli-no-opencode2-exec-");
    const v1 = join(root, "opencode");
    writeFileSync(v1, "fixture\n", { mode: 0o755 });
    const v2 = writeHostPackage(root, "@opencode-ai/cli", "opencode2", "0.0.0-beta-fixture");
    const probed: string[] = [];

    detectOpenCodeHostGeneration({
      findExecutable: (name) => (name === "opencode" ? v1 : v2),
      probeV1Version: (executable) => {
        probed.push(executable);
        return MODERN_V1_VERSION;
      },
    });

    expect(probed).toEqual([v1]);
    expect(probed).not.toContain(v2);
  });

  test("operator canary rejects a host probe that changes the live database", () => {
    const root = tempRoot("aft-cli-host-probe-canary-");
    const operatorHome = join(root, "operator");
    const operatorData = join(operatorHome, ".local", "share", "opencode");
    mkdirSync(operatorData, { recursive: true });
    const database = join(operatorData, "opencode.db");
    writeFileSync(database, "operator database canary");

    expect(() =>
      probeOpenCodeV1Version("/fixture/opencode", {
        operatorHome,
        tempParent: root,
        spawn: () => {
          writeFileSync(database, "mutated");
          return { status: 0, stdout: MODERN_V1_VERSION };
        },
      }),
    ).toThrow("changed the operator database or log directory");
  });

  test("isolates the fallback host probe behind standalone and operator canaries", () => {
    const root = tempRoot("aft-cli-host-probe-");
    const operatorHome = join(root, "operator");
    const operatorData = join(operatorHome, ".local", "share", "opencode");
    const operatorLog = join(operatorData, "log");
    mkdirSync(operatorLog, { recursive: true });
    writeFileSync(join(operatorData, "opencode.db"), "operator database canary");
    writeFileSync(join(operatorLog, "host.log"), "operator log canary");
    const calls: Array<{ executable: string; args: string[]; options: Record<string, unknown> }> =
      [];

    const version = probeOpenCodeV1Version("/fixture/opencode", {
      operatorHome,
      tempParent: root,
      env: { PATH: process.env.PATH },
      spawn: (executable, args, options) => {
        calls.push({ executable, args, options });
        const env = options.env;
        for (const key of [
          "HOME",
          "XDG_CONFIG_HOME",
          "XDG_DATA_HOME",
          "XDG_STATE_HOME",
          "XDG_CACHE_HOME",
        ]) {
          expect(env[key]?.startsWith(root)).toBe(true);
        }
        return { status: 0, stdout: `${MODERN_V1_VERSION}\n` };
      },
    });

    expect(version).toBe(MODERN_V1_VERSION);
    expect(calls).toHaveLength(1);
    expect(calls[0]?.executable).toBe("/fixture/opencode");
    expect(calls[0]?.args).toEqual(["--standalone", "--version"]);
    expect(readFileSync(join(operatorData, "opencode.db"), "utf8")).toBe(
      "operator database canary",
    );
    expect(readFileSync(join(operatorLog, "host.log"), "utf8")).toBe("operator log canary");
  });
});

describe("exact OpenCode config pins", () => {
  test("replaces unpinned, ranged, and duplicate entries in place and is idempotent", () => {
    const exact = pinnedPluginEntry(getSelfVersion());
    for (const existing of [
      AFT_OPENCODE_PACKAGE,
      `${AFT_OPENCODE_PACKAGE}@latest`,
      `${AFT_OPENCODE_PACKAGE}@^${getSelfVersion()}`,
      `${AFT_OPENCODE_PACKAGE}@0.0.0-older`,
    ]) {
      const value: Record<string, unknown> = {
        plugin: ["before", existing, `${AFT_OPENCODE_PACKAGE}@latest`, "after"],
      };
      const first = ensurePinnedPluginConfig(value, getSelfVersion());
      expect(first.action).toBe("updated");
      expect(value.plugin).toEqual(["before", exact, "after"]);

      const snapshot = JSON.stringify(value);
      const second = ensurePinnedPluginConfig(value, getSelfVersion());
      expect(second.action).toBe("already_present");
      expect(JSON.stringify(value)).toBe(snapshot);
    }
  });

  test("migrates the plural key to singular for server and TUI-shaped documents", () => {
    for (const otherPlugin of ["server-plugin", "tui-plugin"]) {
      const value: Record<string, unknown> = {
        plugins: [otherPlugin, `${AFT_OPENCODE_PACKAGE}@latest`],
      };

      ensurePinnedPluginConfig(value, getSelfVersion());

      expect(value.plugins).toBeUndefined();
      expect(value.plugin).toEqual([otherPlugin, pinnedPluginEntry(getSelfVersion())]);
    }
  });

  test("setup detects V1 and V2, writes exact singular pins, and is idempotent", async () => {
    for (const generation of ["v1", "v2"] as const) {
      const root = tempRoot(`aft-cli-setup-${generation}-`);
      const adapter = new ConfiguredOpenCodeAdapter(root);
      const lines = captureOutput();
      const options = {
        resolveAdapters: async () => [adapter],
        detectOpenCodeHost: () => detection(generation),
      };

      expect(await runSetup([], options)).toBe(0);
      const serverPath = join(root, "opencode.json");
      const tuiPath = join(root, "tui.json");
      const server = JSON.parse(readFileSync(serverPath, "utf8")) as Record<string, unknown>;
      const tui = JSON.parse(readFileSync(tuiPath, "utf8")) as Record<string, unknown>;
      expect(server).toEqual({ plugin: [pinnedPluginEntry(getSelfVersion())] });
      expect(tui).toEqual({ plugin: [pinnedPluginEntry(getSelfVersion())] });
      expect(server.plugins).toBeUndefined();
      expect(tui.plugins).toBeUndefined();
      expect(lines.join("\n")).toContain(`host generation ${generation === "v1" ? "V1" : "V2"}`);

      const before = [readFileSync(serverPath, "utf8"), readFileSync(tuiPath, "utf8")];
      expect(await runSetup([], options)).toBe(0);
      expect([readFileSync(serverPath, "utf8"), readFileSync(tuiPath, "utf8")]).toEqual(before);
    }
  });

  test("setup refuses every write when host generation is ambiguous", async () => {
    const root = tempRoot("aft-cli-setup-ambiguous-");
    const adapter = new ConfiguredOpenCodeAdapter(root);

    const code = await runSetup([], {
      resolveAdapters: async () => [adapter],
      detectOpenCodeHost: () => detection("ambiguous"),
    });

    expect(code).toBe(1);
    expect(adapter.ensureCalls).toBe(0);
    expect(adapter.hasPluginEntry()).toBe(false);
  });
});

function doctorFixture(root: string): {
  adapter: HarnessAdapter;
  harness: HarnessDiagnostic;
  report: DiagnosticReport;
} {
  const version = getSelfVersion();
  const configPaths: HarnessConfigPaths = {
    configDir: root,
    harnessConfig: join(root, "opencode.json"),
    harnessConfigFormat: "json",
    aftConfig: join(root, "aft.jsonc"),
    aftConfigFormat: "none",
    tuiConfig: join(root, "tui.json"),
    tuiConfigFormat: "none",
  };
  writeFileSync(
    configPaths.harnessConfig,
    JSON.stringify({ plugin: [pinnedPluginEntry(version)] }),
  );
  const adapter: HarnessAdapter = {
    kind: "opencode",
    displayName: "OpenCode",
    pluginPackageName: AFT_OPENCODE_PACKAGE,
    pluginEntryWithVersion: pinnedPluginEntry(version),
    isInstalled: () => true,
    getHostVersion: () => null,
    detectConfigPaths: () => configPaths,
    hasPluginEntry: () => true,
    ensurePluginEntry: async () => ({
      ok: true,
      action: "already_present",
      message: "already present",
      configPath: configPaths.harnessConfig,
    }),
    getPluginCacheInfo: () => ({ path: join(root, "cache"), exists: false }),
    getStorageDir: () => join(root, "storage"),
    getLogFile: () => join(root, "aft-plugin.log"),
    getInstallHint: () => "fixture",
    clearPluginCache: async () => ({ action: "not_found", path: join(root, "cache") }),
  };
  const harness: HarnessDiagnostic = {
    kind: "opencode",
    displayName: "OpenCode",
    hostInstalled: true,
    hostVersion: null,
    pluginRegistered: true,
    configPaths,
    aftConfig: { exists: false, enabled: true, flags: {} },
    pluginCache: { path: join(root, "cache"), exists: false },
    storageDir: { path: join(root, "storage"), exists: false, accessible: false, sizesByKey: {} },
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
      requirement: "fixture",
    },
    logFile: { path: join(root, "aft-plugin.log"), exists: false, sizeKb: 0 },
  };
  return {
    adapter,
    harness,
    report: {
      timestamp: new Date(0).toISOString(),
      platform: process.platform,
      arch: process.arch,
      nodeVersion: process.version,
      cliVersion: version,
      binaryVersion: version,
      harnesses: [harness],
      binaryCache: { path: join(root, "binary-cache"), versions: [], totalSize: 0 },
      lspCache: {
        npm: { path: join(root, "npm-cache"), entries: [], totalSize: 0 },
        github: { path: join(root, "github-cache"), entries: [], totalSize: 0 },
        totalSize: 0,
      },
    },
  };
}

describe("OpenCode doctor generation and load path", () => {
  test("keeps the reported load path inside the closed enum and detects mismatches", () => {
    const root = tempRoot("aft-cli-doctor-load-");
    const fixture = doctorFixture(root);
    writeFileSync(fixture.harness.logFile.path, "load_path=export-server-v1\n");

    const result = diagnoseOpenCodeLoad({
      detection: detection("v2", "bun"),
      configPath: fixture.harness.configPaths.harnessConfig,
      logPath: fixture.harness.logFile.path,
      pluginCachePath: fixture.harness.pluginCache.path,
    });

    expect(OPENCODE_LOAD_PATHS).toContain(result.takenLoadPath);
    expect(result.takenLoadPath).toBe("export-server-v1");
    expect(result.expectedLoadPath).toBe("export-server-effect-bun");
    expect(result.problems).toHaveLength(1);
  });

  test("derives the V1 root fallback from the installed plugin manifest", () => {
    const root = tempRoot("aft-cli-doctor-root-fallback-");
    const pluginRoot = join(root, "plugin");
    mkdirSync(pluginRoot, { recursive: true });
    writeFileSync(
      join(pluginRoot, "package.json"),
      JSON.stringify({ name: AFT_OPENCODE_PACKAGE, version: getSelfVersion() }),
    );
    const configPath = join(root, "opencode.json");
    writeFileSync(configPath, JSON.stringify({ plugin: [pathToFileURL(pluginRoot).href] }));

    const result = diagnoseOpenCodeLoad({
      detection: detection("v1"),
      configPath,
      logPath: join(root, "missing.log"),
      pluginCachePath: join(root, "missing-cache"),
    });

    expect(result.takenLoadPath).toBe("root-default");
    expect(result.expectedLoadPath).toBe("export-server-v1");
    expect(result.pluginVersion).toBe(getSelfVersion());
    expect(result.problems).toHaveLength(1);
  });

  test("reports the configured V2 plugin version and exits non-zero on a path mismatch", async () => {
    const root = tempRoot("aft-cli-doctor-command-");
    const fixture = doctorFixture(root);
    writeFileSync(fixture.harness.logFile.path, "load path: root-default\n");
    const lines = captureOutput();

    const code = await runDoctor({
      clear: false,
      fix: false,
      force: false,
      issue: false,
      argv: [],
      resolveAdapters: async () => [fixture.adapter],
      collectDiagnostics: async () => fixture.report,
      collectRemovalHealth: async () => ({ available: false, message: "fixture" }),
      detectOpenCodeHost: () => detection("v2", "node"),
    });

    const output = lines.join("\n");
    expect(code).toBe(1);
    expect(output).toContain("host generation: V2");
    expect(output).toContain("load path: root-default");
    expect(output).toContain("expected load path: export-server-effect-node");
    expect(output).toContain(`plugin version: ${getSelfVersion()}`);
  });

  test("doctor --fix reports both generations and refuses configuration writes", async () => {
    const root = tempRoot("aft-cli-doctor-fix-ambiguous-");
    const fixture = doctorFixture(root);
    let ensureCalls = 0;
    fixture.adapter.ensurePluginEntry = async () => {
      ensureCalls += 1;
      return {
        ok: true,
        action: "updated",
        message: "unexpected write",
        configPath: fixture.harness.configPaths.harnessConfig,
      };
    };
    const lines = captureOutput();

    const code = await runDoctor({
      clear: false,
      fix: true,
      force: false,
      issue: false,
      argv: ["--fix", "--yes"],
      resolveAdapters: async () => [fixture.adapter],
      detectOpenCodeHost: () => detection("ambiguous"),
    });

    expect(code).toBe(1);
    expect(ensureCalls).toBe(0);
    expect(lines.join("\n")).toContain("host generation ambiguous (V1, V2)");
    expect(lines.join("\n")).toContain("no changes made");
  });

  test("ambiguous doctor output reports V1 and V2 and remains non-zero", async () => {
    const root = tempRoot("aft-cli-doctor-ambiguous-");
    const fixture = doctorFixture(root);
    const lines = captureOutput();

    const code = await runDoctor({
      clear: false,
      fix: false,
      force: false,
      issue: false,
      argv: [],
      resolveAdapters: async () => [fixture.adapter],
      collectDiagnostics: async () => fixture.report,
      collectRemovalHealth: async () => ({ available: false, message: "fixture" }),
      detectOpenCodeHost: () => detection("ambiguous"),
    });

    expect(code).toBe(1);
    expect(lines.join("\n")).toContain("host generation: ambiguous (V1, V2)");
  });
});
