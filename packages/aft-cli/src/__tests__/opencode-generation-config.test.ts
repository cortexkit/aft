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
import { AFT_SCHEMA_URL } from "../lib/jsonc.js";
import { getOnnxLibraryName, ONNX_RUNTIME_VERSION } from "../lib/onnx.js";
import { runOnnxFix } from "../lib/onnx-fix.js";
import { getSelfVersion } from "../lib/self-version.js";
import {
  detectOpenCodeHostGeneration,
  type OpenCodeHostDetection,
  parseHostVersion,
  probeOpenCodeV1Version,
} from "../setup/host-generation.js";
import {
  AFT_OPENCODE_PACKAGE,
  ensurePinnedPluginConfig,
  MODERN_V1_VERSION,
  openCodePluginKey,
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
  name: "opencode-ai" | "@opencode-ai/cli" | "@opencode/cli",
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

  /** Keep every doctor run inside the fixture root, never the operator's storage. */
  override getStorageDir(): string {
    return this.root;
  }

  override async ensurePluginEntry(): Promise<PluginEntryResult> {
    this.ensureCalls += 1;
    return super.ensurePluginEntry();
  }
}

function readConfig(path: string): Record<string, unknown> {
  return JSON.parse(readFileSync(path, "utf8")) as Record<string, unknown>;
}

describe("OpenCode generation detection", () => {
  test("reads the modern V1 pin from the repository source of truth", () => {
    const repositoryPin = readFileSync(
      new URL("../../../../.github/opencode-version.txt", import.meta.url),
      "utf8",
    ).trim();
    expect(MODERN_V1_VERSION).toBe(repositoryPin);
  });

  // A clean GA install is the common case for a new user and it used to be
  // refused: `@opencode/cli` maps `opencode` and `opencode2` onto one file, so
  // the detector described a single host twice and called it ambiguous.
  test("reads a GA-only install as one V2 host rather than both generations", () => {
    const root = tempRoot("aft-cli-host-ga-");
    const ga = writeHostPackage(root, "@opencode/cli", "opencode", "2.0.11");
    const probed: string[] = [];

    const result = detectOpenCodeHostGeneration({
      findExecutable: () => ga,
      probeV1Version: (executable) => {
        probed.push(executable);
        return null;
      },
    });

    expect(result.status).toBe("v2");
    expect(result.generations).toEqual(["v2"]);
    expect(result.evidence).toHaveLength(1);
    expect(result.evidence[0]?.version).toBe("2.0.11");
    expect(probed).toEqual([]);
  });

  // Desktop and other non-npm installs have no package.json beside the binary,
  // so the generation has to come from the version alone.
  test("reads a GA version without package metadata as V2", () => {
    const root = tempRoot("aft-cli-host-ga-bare-");
    const bin = join(root, "bin", "opencode");
    mkdirSync(join(root, "bin"), { recursive: true });
    writeFileSync(bin, "host fixture\n", { mode: 0o755 });

    const result = detectOpenCodeHostGeneration({
      findExecutable: (name) => (name === "opencode" ? bin : null),
      probeV1Version: () => "2.0.11",
    });

    expect(result.status).toBe("v2");
    expect(result.evidence).toHaveLength(1);
  });

  // The refusal still has to fire for the state it was written for: two
  // different hosts, each its own file.
  test("still reports both generations when V1 and V2 are separate installs", () => {
    const root = tempRoot("aft-cli-host-both-");
    const v1 = writeHostPackage(root, "opencode-ai", "opencode", MODERN_V1_VERSION);
    const v2 = writeHostPackage(root, "@opencode/cli", "opencode2", "2.0.11");

    const result = detectOpenCodeHostGeneration({
      findExecutable: (name) => (name === "opencode" ? v1 : v2),
      probeV1Version: () => null,
    });

    expect(result.status).toBe("ambiguous");
    expect(result.generations).toEqual(["v1", "v2"]);
    expect(result.evidence).toHaveLength(2);
  });

  // The shape reported from a machine that never had V1. The standalone
  // installer writes the real OpenCode 2 binary to ~/.opencode/bin/opencode
  // (168 MB there) and a 47-byte `opencode2` shim beside it, so the two names
  // are different files and no npm package metadata sits beside either. That
  // leaves the version the host prints as the only signal, and it prints
  // `opencode v2.0.12` — recorded from the affected machine; a local
  // @opencode/cli install prints the same shape at its own version. Reading
  // that string as a version is what failed, and the machine came out as
  // ambiguous (V1, V2) with V1 nowhere on it.
  test("reads a standalone V2 binary with a separate opencode2 shim as V2", () => {
    const root = tempRoot("aft-cli-host-standalone-v2-");
    const binDir = join(root, ".opencode", "bin");
    mkdirSync(binDir, { recursive: true });
    const host = join(binDir, "opencode");
    const shim = join(binDir, "opencode2");
    writeFileSync(host, "compiled host fixture\n", { mode: 0o755 });
    writeFileSync(shim, `#!/bin/sh\nexec "$(dirname "$0")/opencode" "$@"\n`, { mode: 0o755 });
    const probed: string[] = [];

    const result = detectOpenCodeHostGeneration({
      findExecutable: (name) => (name === "opencode" ? host : shim),
      probeV1Version: (executable) => {
        probed.push(executable);
        return probeOpenCodeV1Version(executable, {
          operatorHome: join(root, "operator"),
          tempParent: root,
          spawn: () => ({ status: 0, stdout: "opencode v2.0.12\n" }),
        });
      },
    });

    expect(result.status).toBe("v2");
    expect(result.generations).toEqual(["v2"]);
    expect(result.evidence.map((item) => item.version)).toEqual(["2.0.12", null]);
    // The shim is never executed; only the host under the plain name is asked.
    expect(probed).toEqual([host]);
  });

  // The generation must not depend on who trimmed the string. A version that
  // reaches the rule still decorated (program name, leading `v`) decides the
  // generation the same way, whichever executable name reported it — including
  // the pre-rename betas, whose `0.0.0-beta-<n>` shape is checked from the
  // start of the string and so only matches once the prefix is gone.
  test("classifies a decorated version string as V2 wherever it came from", () => {
    const root = tempRoot("aft-cli-host-decorated-version-");
    const host = join(root, "bin", "opencode");
    mkdirSync(join(root, "bin"), { recursive: true });
    writeFileSync(host, "compiled host fixture\n", { mode: 0o755 });
    const detect = (reported: string): OpenCodeHostDetection =>
      detectOpenCodeHostGeneration({
        findExecutable: (name) => (name === "opencode" ? host : null),
        probeV1Version: () => reported,
      });

    const ga = detect("opencode v2.0.12");
    expect(ga.status).toBe("v2");
    expect(ga.evidence).toHaveLength(1);

    expect(detect("opencode v0.0.0-beta-19234").status).toBe("v2");
    expect(detect("0.0.0-dev-42").status).toBe("v2");
    expect(detect(`opencode v${MODERN_V1_VERSION}`).status).toBe("v1");
  });

  // Both generations answer `--standalone --version` on stdout and exit 0, in
  // two different formats. Byte-for-byte captures: `1.18.30\n` from the
  // standalone V1 binary, `opencode v2.0.11\n` from @opencode/cli@2.0.11.
  test("parses the version out of what each generation actually prints", () => {
    const root = tempRoot("aft-cli-host-version-output-");
    const probe = (stdout: string): string | null =>
      probeOpenCodeV1Version("/fixture/opencode", {
        operatorHome: join(root, "operator"),
        tempParent: root,
        spawn: () => ({ status: 0, stdout }),
      });

    expect(probe("1.18.30\n")).toBe("1.18.30");
    expect(probe("opencode v2.0.11\n")).toBe("2.0.11");
    expect(probe("\n")).toBeNull();

    expect(parseHostVersion("2.0.12")).toBe("2.0.12");
    expect(parseHostVersion("v2.0.12")).toBe("2.0.12");
    expect(parseHostVersion("opencode v2.0.12")).toBe("2.0.12");
    expect(parseHostVersion("0.0.0-beta-19234")).toBe("0.0.0-beta-19234");
    expect(parseHostVersion("opencode v0.0.0-beta-19234")).toBe("0.0.0-beta-19234");
    expect(parseHostVersion("0.0.0-dev-42")).toBe("0.0.0-dev-42");
    expect(parseHostVersion("no version here")).toBeNull();
  });

  // A real dual install does not always carry metadata on both sides: V1 from
  // the standalone installer has none, so its generation also comes from the
  // probed version. That must still read as two generations.
  test("still reports both generations for a standalone V1 beside an npm V2", () => {
    const root = tempRoot("aft-cli-host-both-standalone-");
    const v1 = join(root, ".opencode", "bin", "opencode");
    mkdirSync(join(root, ".opencode", "bin"), { recursive: true });
    writeFileSync(v1, "compiled host fixture\n", { mode: 0o755 });
    const v2 = writeHostPackage(root, "@opencode/cli", "opencode2", "2.0.11");

    const result = detectOpenCodeHostGeneration({
      findExecutable: (name) => (name === "opencode" ? v1 : v2),
      probeV1Version: (executable) =>
        probeOpenCodeV1Version(executable, {
          operatorHome: join(root, "operator"),
          tempParent: root,
          spawn: () => ({ status: 0, stdout: `${MODERN_V1_VERSION}\n` }),
        }),
    });

    expect(result.status).toBe("ambiguous");
    expect(result.generations).toEqual(["v1", "v2"]);
    expect(result.evidence[0]?.modernV1).toBe(true);
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
        env: { AFT_LOAD_MATRIX_ALLOW_LIVE_OPERATOR: "0" },
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
  // The rename is the whole point: a V1 host reads `plugin`, a V2 host reads
  // `plugins`, and an entry under the other name is not loaded at all.
  test("writes the entry under the key the detected generation reads", () => {
    for (const generation of ["v1", "v2"] as const) {
      const key = openCodePluginKey(generation);
      const other = key === "plugin" ? "plugins" : "plugin";
      const value: Record<string, unknown> = {};

      const update = ensurePinnedPluginConfig(value, getSelfVersion(), () => false, generation);

      expect(update.key).toBe(key);
      expect(value[key]).toEqual([pinnedPluginEntry(getSelfVersion())]);
      expect(value[other]).toBeUndefined();
    }
  });

  // One machine can run both hosts against one config file. Folding the other
  // generation's key into this one, or deleting it, unregisters AFT on the
  // host that is not being configured.
  test("leaves the other generation's key and its entries untouched", () => {
    for (const generation of ["v1", "v2"] as const) {
      const other = generation === "v2" ? "plugin" : "plugins";
      const value: Record<string, unknown> = {
        [other]: ["other-plugin", `${AFT_OPENCODE_PACKAGE}@0.0.0-older`],
      };

      ensurePinnedPluginConfig(value, getSelfVersion(), () => false, generation);

      expect(value[other]).toEqual(["other-plugin", `${AFT_OPENCODE_PACKAGE}@0.0.0-older`]);
      expect(value[openCodePluginKey(generation)]).toEqual([pinnedPluginEntry(getSelfVersion())]);
    }
  });

  test("pins in place and stays idempotent under both generations", () => {
    const exact = pinnedPluginEntry(getSelfVersion());
    for (const generation of ["v1", "v2"] as const) {
      const key = openCodePluginKey(generation);
      for (const existing of [
        AFT_OPENCODE_PACKAGE,
        `${AFT_OPENCODE_PACKAGE}@latest`,
        `${AFT_OPENCODE_PACKAGE}@^${getSelfVersion()}`,
        `${AFT_OPENCODE_PACKAGE}@0.0.0-older`,
      ]) {
        const value: Record<string, unknown> = {
          [key]: ["before", existing, `${AFT_OPENCODE_PACKAGE}@latest`, "after"],
        };
        const first = ensurePinnedPluginConfig(value, getSelfVersion(), () => false, generation);
        expect(first.action).toBe("updated");
        expect(value[key]).toEqual(["before", exact, "after"]);

        const snapshot = JSON.stringify(value);
        const second = ensurePinnedPluginConfig(value, getSelfVersion(), () => false, generation);
        expect(second.action).toBe("already_present");
        expect(JSON.stringify(value)).toBe(snapshot);
      }
    }
  });

  // A developer who registered a checkout keeps that checkout after a host
  // upgrade, in the entry shape the new host can read: V1 pairs options in a
  // tuple, V2 in a `package`/`options` object. The entry MOVES rather than
  // being copied: GA's compatibility path converts a V1 `plugin` list into
  // `plugins`, so the same checkout under both keys is ambiguous. Every other
  // plugin's entry stays exactly where the user wrote it.
  test("moves a local checkout to the host's key in that key's entry shape", () => {
    const local = "/dev/aft/packages/opencode-plugin";
    const value: Record<string, unknown> = {
      plugin: ["other-plugin", [local, { enabled: true }]],
    };

    const update = ensurePinnedPluginConfig(
      value,
      getSelfVersion(),
      (entry) => entry === local,
      "v2",
    );

    expect(update.action).toBe("added");
    expect(value.plugins).toEqual([{ package: local, options: { enabled: true } }]);
    expect(value.plugin).toEqual(["other-plugin"]);
  });

  test("rewrites AFT's own V1 tuple under the V2 key and leaves another plugin's alone", () => {
    const local = "/dev/aft/packages/opencode-plugin";
    const value: Record<string, unknown> = {
      plugins: [
        ["other-plugin", { enabled: true }],
        [local, { enabled: false }],
      ],
    };

    const update = ensurePinnedPluginConfig(
      value,
      getSelfVersion(),
      (entry) => entry === local,
      "v2",
    );

    expect(update.changed).toBe(true);
    expect(value.plugins).toEqual([
      ["other-plugin", { enabled: true }],
      { package: local, options: { enabled: false } },
    ]);
  });

  test("setup detects V1 and V2, writes each host's key, and is idempotent", async () => {
    for (const generation of ["v1", "v2"] as const) {
      const key = openCodePluginKey(generation);
      const other = key === "plugin" ? "plugins" : "plugin";
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
      const server = readConfig(serverPath);
      expect(server).toEqual({ [key]: [pinnedPluginEntry(getSelfVersion())] });
      expect(server[other]).toBeUndefined();
      // V1 needs the TUI sidebar registered in its own config; OpenCode 2
      // loads that plugin from the package's `tui` entrypoint and never reads
      // a TUI config file, so writing one there would leave a file the host
      // ignores. GA 2.0.14 contains no reference to tui.json or tui.jsonc.
      if (generation === "v1") {
        const tui = readConfig(tuiPath);
        expect(tui).toEqual({ [key]: [pinnedPluginEntry(getSelfVersion())] });
        expect(tui[other]).toBeUndefined();
      } else {
        expect(existsSync(tuiPath)).toBe(false);
      }
      expect(adapter.hasPluginEntry()).toBe(true);
      // Setup names the host the way a user does ("OpenCode 1"), not by the
      // internal generation label the doctor report uses.
      expect(lines.join("\n")).toContain(`Found OpenCode ${generation === "v1" ? "1" : "2"}.`);

      // Idempotence covers whichever files this generation actually owns: on
      // V2 the TUI config is deliberately absent, and re-running setup must
      // keep it absent rather than creating one on the second pass.
      const paths = generation === "v1" ? [serverPath, tuiPath] : [serverPath];
      const before = paths.map((path) => readFileSync(path, "utf8"));
      expect(await runSetup([], options)).toBe(0);
      expect(paths.map((path) => readFileSync(path, "utf8"))).toEqual(before);
      if (generation === "v2") expect(existsSync(tuiPath)).toBe(false);
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

function doctorFixture(
  root: string,
  pluginEntry = pinnedPluginEntry(getSelfVersion()),
  generation: OpenCodeHostDetection["status"] = "v2",
): {
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
    JSON.stringify({ [openCodePluginKey(generation)]: [pluginEntry] }),
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
      autoDownloadable: true,
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
    writeFileSync(fixture.harness.logFile.path, "load_path=root-default\n");

    const result = diagnoseOpenCodeLoad({
      detection: detection("v2", "bun"),
      configPath: fixture.harness.configPaths.harnessConfig,
      logPath: fixture.harness.logFile.path,
      pluginCachePath: fixture.harness.pluginCache.path,
    });

    expect(OPENCODE_LOAD_PATHS).toContain(result.takenLoadPath);
    expect(result.takenLoadPath).toBe("root-default");
    expect(result.expectedLoadPath).toBe("export-server-effect-bun");
    expect(result.problems).toHaveLength(1);
  });

  test("treats a V1 server-export manifest as a root-default load without a log line", () => {
    const root = tempRoot("aft-cli-doctor-root-fallback-");
    const pluginRoot = join(root, "plugin");
    mkdirSync(pluginRoot, { recursive: true });
    writeFileSync(
      join(pluginRoot, "package.json"),
      JSON.stringify({
        name: AFT_OPENCODE_PACKAGE,
        version: getSelfVersion(),
        exports: { ".": "./dist/index.js", "./server": "./dist/server.js" },
        "oc-plugin": ["server"],
      }),
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
    expect(result.expectedLoadPath).toBe("root-default");
    expect(result.pluginVersion).toBe(getSelfVersion());
    expect(result.problems).toEqual([]);
  });

  test("accepts V1 latest registration on a logged root-default load", async () => {
    const root = tempRoot("aft-cli-doctor-v1-latest-");
    const fixture = doctorFixture(root, `${AFT_OPENCODE_PACKAGE}@latest`, "v1");
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
      detectOpenCodeHost: () => detection("v1"),
    });

    const output = lines.join("\n");
    expect(code).toBe(0);
    expect(output).toContain("host generation: V1");
    expect(output).toContain("load path: root-default");
    expect(output).not.toContain("to pin it to");
  });

  test("reports every plan feature, and a rejected config as a problem", async () => {
    const root = tempRoot("aft-cli-doctor-features-");
    const fixture = doctorFixture(root, `${AFT_OPENCODE_PACKAGE}@latest`, "v1");
    writeFileSync(fixture.harness.logFile.path, "load path: root-default\n");
    const plan = {
      plan_version: 1,
      features: [
        {
          id: "indexes.trigram",
          kind: "index",
          group: "Indexes",
          order: 24,
          label: "Trigram index",
          description: "d",
          binding: { path: "indexes.trigram", tool_name: null },
          default: true,
          configured: true,
          source: "default",
          proposed: true,
          effective: "unavailable",
          reason: "default",
          available: false,
          unavailable_reason: "runtime_not_observed",
          cost_note: null,
          prerequisites: [],
        },
      ],
    };
    const base = {
      clear: false,
      fix: false,
      force: false,
      issue: false,
      argv: ["--harness", "opencode"],
      resolveAdapters: async () => [fixture.adapter],
      collectDiagnostics: async () => fixture.report,
      collectRemovalHealth: async () => ({ available: false, message: "fixture" }),
      detectOpenCodeHost: () => detection("v1"),
    };
    const calls: string[][] = [];
    let lines = captureOutput();
    const code = await runDoctor({
      ...base,
      runNative: (args) => {
        calls.push(args);
        return { ok: true, stdout: JSON.stringify(plan), stderr: "", status: 0 };
      },
    });
    expect(code).toBe(0);
    expect(calls).toEqual([["setup", "--plan", "--harness", "opencode"]]);
    expect(lines.join("\n")).toContain(
      "indexes.trigram: unavailable — configured on (default); reason: default; unavailable: runtime_not_observed",
    );

    lines = captureOutput();
    const rejected = await runDoctor({
      ...base,
      runNative: () => ({
        ok: false,
        stdout: "",
        stderr:
          "removed_config_key:gh_read:use:github.read\nAFT cannot load this configuration. Run `aft doctor --fix` to migrate it, then rerun setup.",
        status: 1,
      }),
    });
    expect(rejected).toBe(1);
    expect(lines.join("\n")).toContain("removed_config_key:gh_read:use:github.read");
  });

  test("accepts an explicit semver registration on V1", () => {
    const root = tempRoot("aft-cli-doctor-v1-semver-");
    const fixture = doctorFixture(root, pinnedPluginEntry(getSelfVersion()), "v1");
    writeFileSync(fixture.harness.logFile.path, "load path: root-default\n");

    const result = diagnoseOpenCodeLoad({
      detection: detection("v1"),
      configPath: fixture.harness.configPaths.harnessConfig,
      logPath: fixture.harness.logFile.path,
      pluginCachePath: fixture.harness.pluginCache.path,
      expectedPluginEntry: `${AFT_OPENCODE_PACKAGE}@latest`,
      acceptExplicitPluginVersion: true,
    });

    expect(result.problems).toEqual([]);
  });

  test("still requires the exact plugin pin on V2", async () => {
    const root = tempRoot("aft-cli-doctor-v2-latest-");
    const fixture = doctorFixture(root, `${AFT_OPENCODE_PACKAGE}@latest`);
    writeFileSync(fixture.harness.logFile.path, "load path: export-server-effect-node\n");
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
    expect(output).toContain(
      // Reworded for users: the line names why the entry is a problem and the
      // exact entry doctor --fix writes, instead of "is not the required exact pin".
      `the plugin entry ${AFT_OPENCODE_PACKAGE}@latest follows the newest release, which can differ from this CLI and its binary; run \`npx @cortexkit/aft doctor --fix\` to pin it to ${pinnedPluginEntry(getSelfVersion())}`,
    );
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

  test("doctor --fix under an ambiguous host repairs ONNX and skips only the config write", async () => {
    const root = tempRoot("aft-cli-doctor-fix-ambiguous-");
    const fixture = doctorFixture(root);
    let ensureCalls = 0;
    fixture.adapter.hasPluginEntry = () => false;
    fixture.adapter.ensurePluginEntry = async () => {
      ensureCalls += 1;
      return {
        ok: true,
        action: "updated",
        message: "unexpected write",
        configPath: fixture.harness.configPaths.harnessConfig,
      };
    };
    // The state the reporting machine was in: an unregistered plugin (which
    // needs the generation to write) and a missing ONNX Runtime (which does
    // not). Only the first may be withheld.
    fixture.harness.pluginRegistered = false;
    fixture.harness.onnxRuntime = { ...fixture.harness.onnxRuntime, required: true };
    const installedLibrary = join(
      fixture.harness.storageDir.path,
      "onnxruntime",
      ONNX_RUNTIME_VERSION,
      getOnnxLibraryName(),
    );
    const lines = captureOutput();

    const code = await runDoctor({
      clear: false,
      fix: true,
      force: false,
      issue: false,
      argv: ["--fix", "--yes"],
      resolveAdapters: async () => [fixture.adapter],
      collectDiagnostics: async () => fixture.report,
      detectOpenCodeHost: () => detection("ambiguous"),
      // The real repair runs; only the download itself is stubbed, so the
      // staging, swap and published path are all exercised.
      applyOnnxFix: (adapters, report, options) =>
        runOnnxFix(adapters, report, {
          ...options,
          ensureFn: async (storageDir) => {
            const directory = join(storageDir, "onnxruntime", ONNX_RUNTIME_VERSION);
            mkdirSync(directory, { recursive: true });
            const library = join(directory, getOnnxLibraryName());
            writeFileSync(library, "fixture runtime\n");
            return library;
          },
        }),
    });

    const output = lines.join("\n");
    const appliedLine = output.split("\n").find((line) => line.includes("Applied:")) ?? "";
    expect(ensureCalls).toBe(0);
    expect(existsSync(installedLibrary)).toBe(true);
    expect(output).toContain("host generation ambiguous (V1, V2)");
    expect(appliedLine).toContain("ONNX Runtime install");
    expect(appliedLine).toContain("AFT config $schema");
    expect(output).toContain("Skipped: plugin registration in the OpenCode config");
    // The withheld plugin-registration write is never offered as a planned
    // change either.
    expect(output).not.toContain(`to ${fixture.harness.configPaths.harnessConfig}`);
    expect(output).not.toContain("no changes made");
    expect(code).toBe(1);
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

/**
 * Run `doctor --fix` against a real adapter over `config`, and return the
 * config file as the run left it. The AFT config is pre-seeded with its schema
 * and storage points at the fixture root so the run has nothing else to
 * change: the subject is only what --fix does to plugin registration.
 */
async function runFixOverConfig(
  label: string,
  generation: "v1" | "v2",
  config: Record<string, unknown>,
): Promise<{
  root: string;
  adapter: ConfiguredOpenCodeAdapter;
  server: Record<string, unknown>;
  output: string;
}> {
  const root = tempRoot(label);
  const fixture = doctorFixture(root, pinnedPluginEntry(getSelfVersion()), generation);
  writeFileSync(fixture.harness.configPaths.harnessConfig, JSON.stringify(config, null, 2));
  writeFileSync(fixture.harness.configPaths.aftConfig, JSON.stringify({ $schema: AFT_SCHEMA_URL }));
  const adapter = new ConfiguredOpenCodeAdapter(root);
  const lines = captureOutput();

  await runDoctor({
    clear: false,
    fix: true,
    force: false,
    issue: false,
    argv: ["--fix", "--yes"],
    resolveAdapters: async () => [adapter],
    collectDiagnostics: async () => fixture.report,
    collectRemovalHealth: async () => ({ available: false, message: "fixture" }),
    detectOpenCodeHost: () => detection(generation),
  });

  return {
    root,
    adapter,
    server: readConfig(fixture.harness.configPaths.harnessConfig),
    output: lines.join("\n"),
  };
}

describe("OpenCode plugin registration follows the host's key", () => {
  test("a V2 host reads `plugins` and a V1 host reads `plugin`", () => {
    const root = tempRoot("aft-cli-registered-key-");
    const entry = pinnedPluginEntry(getSelfVersion());
    writeFileSync(join(root, "opencode.json"), JSON.stringify({ plugins: [entry] }));

    const registered = (generation: "v1" | "v2"): boolean => {
      const adapter = new ConfiguredOpenCodeAdapter(root);
      adapter.useHostDetection(detection(generation));
      return adapter.hasPluginEntry();
    };

    // The GA report this fixes: a working `plugins` entry read as unregistered.
    expect(registered("v2")).toBe(true);
    // And the mirror image, so the answer is the key and not a new default.
    expect(registered("v1")).toBe(false);
  });

  test("a V2 registration in the object entry shape is recognised", () => {
    const root = tempRoot("aft-cli-registered-object-");
    const pluginRoot = join(root, "plugin");
    mkdirSync(pluginRoot, { recursive: true });
    writeFileSync(
      join(pluginRoot, "package.json"),
      JSON.stringify({ name: AFT_OPENCODE_PACKAGE, version: getSelfVersion() }),
    );
    const configPath = join(root, "opencode.json");
    writeFileSync(
      configPath,
      JSON.stringify({ plugins: [{ package: pluginRoot, options: { enabled: true } }] }),
    );

    const result = diagnoseOpenCodeLoad({
      detection: detection("v2", "node"),
      configPath,
      logPath: join(root, "missing.log"),
      pluginCachePath: join(root, "missing-cache"),
    });

    expect(result.pluginVersion).toBe(getSelfVersion());
    expect(result.problems).toEqual([]);
  });

  test("doctor --fix keeps a V2 `plugins` registration and writes no `plugin` key", async () => {
    const entry = pinnedPluginEntry(getSelfVersion());
    const result = await runFixOverConfig("aft-cli-fix-v2-", "v2", {
      plugins: ["other-plugin", entry],
    });

    expect(result.server).toEqual({ plugins: ["other-plugin", entry] });
    expect(result.server.plugin).toBeUndefined();
    expect(result.adapter.hasPluginEntry()).toBe(true);
    // No TUI config on V2: the host loads that plugin from the package's own
    // `tui` entrypoint and reads no TUI config file, so creating one would be
    // a repair that writes something OpenCode 2 ignores.
    expect(existsSync(join(result.root, "tui.json"))).toBe(false);
  });

  test("doctor --fix keeps a V1 `plugin` registration and writes no `plugins` key", async () => {
    const entry = pinnedPluginEntry(getSelfVersion());
    const result = await runFixOverConfig("aft-cli-fix-v1-", "v1", {
      plugin: ["other-plugin", entry],
    });

    expect(result.server).toEqual({ plugin: ["other-plugin", entry] });
    expect(result.server.plugins).toBeUndefined();
    expect(result.adapter.hasPluginEntry()).toBe(true);
  });

  test("doctor --fix leaves both generations' keys intact in one config", async () => {
    const entry = pinnedPluginEntry(getSelfVersion());
    // The V2 entry is stale on purpose. A config that already holds the exact
    // pin is never rewritten, so only a config that forces a write can show
    // what that write does to the other generation's key.
    const result = await runFixOverConfig("aft-cli-fix-both-keys-", "v2", {
      plugin: ["v1-only-plugin", entry],
      plugins: ["v2-only-plugin", `${AFT_OPENCODE_PACKAGE}@latest`],
    });

    // Both hosts may be run against this file, so --fix repairs the key of the
    // host in front of it and leaves the other host's registration alone.
    expect(result.server).toEqual({
      plugin: ["v1-only-plugin", entry],
      plugins: ["v2-only-plugin", entry],
    });
  });

  test("doctor --fix on a V1 host leaves a V2 `plugins` registration intact", async () => {
    const entry = pinnedPluginEntry(getSelfVersion());
    const result = await runFixOverConfig("aft-cli-fix-both-keys-v1-", "v1", {
      plugin: ["v1-only-plugin", `${AFT_OPENCODE_PACKAGE}@latest`],
      plugins: ["v2-only-plugin", entry],
    });

    expect(result.server).toEqual({
      plugin: ["v1-only-plugin", entry],
      plugins: ["v2-only-plugin", entry],
    });
  });

  test("doctor --fix registers under `plugins` without disturbing the V1 entry", async () => {
    const entry = pinnedPluginEntry(getSelfVersion());
    const result = await runFixOverConfig("aft-cli-fix-v1-config-on-v2-", "v2", {
      plugin: ["other-plugin", `${AFT_OPENCODE_PACKAGE}@0.0.0-older`],
    });

    expect(result.server).toEqual({
      plugin: ["other-plugin", `${AFT_OPENCODE_PACKAGE}@0.0.0-older`],
      plugins: [entry],
    });
    expect(result.adapter.hasPluginEntry()).toBe(true);
  });

  test("doctor names the key a V2 host reads when AFT sits under `plugin`", () => {
    const root = tempRoot("aft-cli-doctor-wrong-key-");
    const configPath = join(root, "opencode.json");
    writeFileSync(configPath, JSON.stringify({ plugin: [pinnedPluginEntry(getSelfVersion())] }));

    const result = diagnoseOpenCodeLoad({
      detection: detection("v2", "node"),
      configPath,
      logPath: join(root, "missing.log"),
      pluginCachePath: join(root, "missing-cache"),
    });

    expect(result.problems).toContain(
      "AFT is registered under `plugin`, which a V2 host does not read; run doctor --fix to register it under `plugins`",
    );
  });

  test("doctor reports another plugin's V1 tuple under `plugins` instead of rewriting it", () => {
    const root = tempRoot("aft-cli-doctor-foreign-tuple-");
    const configPath = join(root, "opencode.json");
    const config = {
      plugins: [["other-plugin", { enabled: true }], pinnedPluginEntry(getSelfVersion())],
    };
    writeFileSync(configPath, JSON.stringify(config));

    const result = diagnoseOpenCodeLoad({
      detection: detection("v2", "node"),
      configPath,
      logPath: join(root, "missing.log"),
      pluginCachePath: join(root, "missing-cache"),
    });

    expect(result.problems).toContain(
      "plugin entry other-plugin under `plugins` uses the V1 entry shape, which a V2 host cannot load; left unchanged because AFT did not register it",
    );

    ensurePinnedPluginConfig(config, getSelfVersion(), () => false, "v2");
    expect(config.plugins[0]).toEqual(["other-plugin", { enabled: true }]);
  });

  test("the ambiguous doctor line names the key each host would use", () => {
    const root = tempRoot("aft-cli-doctor-ambiguous-keys-");
    const configPath = join(root, "opencode.json");
    writeFileSync(configPath, JSON.stringify({ plugins: [pinnedPluginEntry(getSelfVersion())] }));

    const result = diagnoseOpenCodeLoad({
      detection: detection("ambiguous"),
      configPath,
      logPath: join(root, "missing.log"),
      pluginCachePath: join(root, "missing-cache"),
    });

    expect(result.problems).toContain(
      "both OpenCode V1 and V2 hosts were detected; refusing configuration writes (a V1 host reads `plugin`, a V2 host reads `plugins`)",
    );
    // No migration advice while the generation is unsettled: either key may be
    // the right one, and writes stay refused.
    expect(result.problems.some((problem) => problem.includes("run doctor --fix"))).toBe(false);
  });
});
