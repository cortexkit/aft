/// <reference path="../bun-test.d.ts" />

// Doctor output for an OpenCode plugin that is older than this CLI. The report
// has to name one remedy, the one `doctor --fix` applies (pin the entry to
// this CLI's exact version and update the plugin), in the OpenCode section and
// under "Issues found" alike. It must never recommend `@latest`, which the
// pinning policy replaces.

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { acquireEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import { OpenCodeAdapter } from "../adapters/opencode.js";
import type { HarnessConfigPaths } from "../adapters/types.js";
import { buildDoctorFixPlan, runDoctor } from "../commands/doctor.js";
import { getBinaryCacheInfo } from "../lib/binary-cache.js";
import {
  collectDiagnostics,
  type DiagnosticReport,
  type HarnessDiagnostic,
  renderDiagnosticsMarkdown,
} from "../lib/diagnostics.js";
import { getSelfVersion } from "../lib/self-version.js";
import { compareSemver } from "../lib/semver.js";
import type { OpenCodeHostDetection } from "../setup/host-generation.js";
import { AFT_OPENCODE_PACKAGE, MODERN_V1_VERSION } from "../setup/opencode-config.js";

const CLI_VERSION = getSelfVersion();
const OLD_PLUGIN = "0.0.1";
const PINNED = `${AFT_OPENCODE_PACKAGE}@${CLI_VERSION}`;
const OLDER_MESSAGE = `Plugin version (${OLD_PLUGIN}) is older than CLI (${CLI_VERSION}). New binary cache won't be used until you update the plugin.`;
const PIN_AND_UPDATE = `Run \`npx @cortexkit/aft doctor --fix\` to pin the plugin entry to ${PINNED} and update the plugin to ${CLI_VERSION}.`;
const UPDATE_ONLY = `Run \`npx @cortexkit/aft doctor --fix\` to update the plugin to ${CLI_VERSION}.`;

const V1: OpenCodeHostDetection = {
  status: "v1",
  generations: ["v1"],
  evidence: [
    {
      generation: "v1",
      executable: "/fixture/opencode",
      version: MODERN_V1_VERSION,
      runtime: "bun",
      modernV1: true,
    },
  ],
};

/** The real adapter, confined to a fixture config directory. */
class FixtureOpenCodeAdapter extends OpenCodeAdapter {
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
      aftConfigFormat: "none",
      tuiConfig,
      tuiConfigFormat: existsSync(tuiConfig) ? "json" : "none",
    };
  }

  override getStorageDir(): string {
    return this.root;
  }
}

const originalLog = console.log;
const originalStdoutWrite = process.stdout.write;
const originalStderrWrite = process.stderr.write;
let releaseEnv: (() => void) | undefined;
let root: string;

beforeEach(async () => {
  root = mkdtempSync(join(tmpdir(), "aft-cli-doctor-skew-"));
  releaseEnv = await acquireEnv({
    OPENCODE_CONFIG_DIR: root,
    XDG_CACHE_HOME: join(root, "xdg-cache"),
    AFT_CACHE_DIR: join(root, "aft-cache"),
  });
});

afterEach(() => {
  console.log = originalLog;
  process.stdout.write = originalStdoutWrite;
  process.stderr.write = originalStderrWrite;
  releaseEnv?.();
  releaseEnv = undefined;
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

function stripAnsi(text: string): string {
  // biome-ignore lint/suspicious/noControlCharactersInRegex: strips terminal colour codes
  return text.replace(/\u001b\[[0-9;]*m/g, "");
}

function occurrences(haystack: string, needle: string): number {
  return haystack.split(needle).length - 1;
}

/** Write the installed plugin's manifest into an OpenCode cache directory. */
function writeInstalledPlugin(cachePath: string, version: string): void {
  const manifestDir = join(cachePath, "node_modules", "@cortexkit", "aft-opencode");
  mkdirSync(manifestDir, { recursive: true });
  writeFileSync(
    join(manifestDir, "package.json"),
    JSON.stringify({ name: AFT_OPENCODE_PACKAGE, version }),
  );
}

/**
 * A V1 OpenCode install whose config registers `entry` and whose package
 * cache holds plugin `cachedVersion`. The harness row comes from the real
 * adapter, so the configured entry reaches the report the way it does on a
 * user's machine.
 */
function fixture(
  entry: string,
  cachedVersion: string,
): {
  adapter: FixtureOpenCodeAdapter;
  report: DiagnosticReport;
} {
  writeFileSync(join(root, "opencode.json"), JSON.stringify({ plugin: [entry] }));
  writeFileSync(join(root, "aft-plugin.log"), "load path: root-default\n");
  const adapter = new FixtureOpenCodeAdapter(root);
  adapter.useHostDetection(V1);
  // OpenCode keeps one cache directory per entry: a package.json asking for
  // the entry's version (or `latest` when it names none) and the installed
  // package under node_modules.
  const cachePath = adapter.getPluginCacheInfo().path;
  const spec = entry.startsWith(`${AFT_OPENCODE_PACKAGE}@`)
    ? entry.slice(AFT_OPENCODE_PACKAGE.length + 1)
    : "latest";
  mkdirSync(cachePath, { recursive: true });
  writeFileSync(
    join(cachePath, "package.json"),
    JSON.stringify({ dependencies: { [AFT_OPENCODE_PACKAGE]: spec } }),
  );
  writeInstalledPlugin(cachePath, cachedVersion);

  const harness: HarnessDiagnostic = {
    kind: "opencode",
    displayName: "OpenCode",
    hostInstalled: true,
    hostVersion: MODERN_V1_VERSION,
    pluginRegistered: true,
    configPaths: adapter.detectConfigPaths(),
    aftConfig: { exists: false, enabled: true, flags: {} },
    pluginCache: adapter.getPluginCacheInfo(),
    storageDir: { path: root, exists: true, accessible: true, sizesByKey: {} },
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
    logFile: { path: join(root, "aft-plugin.log"), exists: true, sizeKb: 0 },
  };
  return {
    adapter,
    report: {
      timestamp: new Date(0).toISOString(),
      platform: process.platform,
      arch: process.arch,
      nodeVersion: process.version,
      cliVersion: CLI_VERSION,
      binaryVersion: CLI_VERSION,
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

async function doctorOutput(entry: string, cachedVersion: string): Promise<[number, string]> {
  const { adapter, report } = fixture(entry, cachedVersion);
  const lines = captureOutput();
  const code = await runDoctor({
    clear: false,
    fix: false,
    force: false,
    issue: false,
    argv: [],
    resolveAdapters: async () => [adapter],
    collectDiagnostics: async () => report,
    collectRemovalHealth: async () => ({ available: false, message: "fixture" }),
    detectOpenCodeHost: () => V1,
    runNative: () => ({
      ok: true,
      stdout: JSON.stringify({ plan_version: 1, features: [] }),
      stderr: "",
      status: 0,
    }),
  });
  return [code, stripAnsi(lines.join("\n"))];
}

/** One line in the OpenCode section and one entry under Issues found, worded alike. */
function expectOneIssue(output: string, message: string, remediation: string): void {
  // Names the missing text and prints the whole report when it is absent.
  expect(output).toContain(`Remediation: ${remediation}`);
  expect(occurrences(output, `[HIGH] OpenCode: ${message}`)).toBe(1);
  expect(occurrences(output, `Remediation: ${remediation}`)).toBe(1);
  expect(occurrences(output, `${message} ${remediation}`)).toBe(1);
  expect(occurrences(output, "is older than CLI")).toBe(2);
  // The separate pin line would be the same root problem reported a second time.
  expect(output).not.toContain("to pin it to");
  expect(output).not.toContain("@latest");
}

describe("doctor: OpenCode plugin older than the CLI", () => {
  test("an unpinned entry with an outdated cached plugin is one issue with the --fix remedy", async () => {
    const [code, output] = await doctorOutput(AFT_OPENCODE_PACKAGE, OLD_PLUGIN);
    const message = `${OLDER_MESSAGE} The plugin entry ${AFT_OPENCODE_PACKAGE} has no version, so OpenCode may load any release of the plugin.`;

    expect(code).toBe(1);
    expectOneIssue(output, message, PIN_AND_UPDATE);
    expect(occurrences(output, "has no version")).toBe(2);

    // The remediation describes what --fix actually plans to do.
    const { adapter, report } = fixture(AFT_OPENCODE_PACKAGE, OLD_PLUGIN);
    const plan = buildDoctorFixPlan([adapter], report).map((item) => item.message);
    expect(plan).toContainEqual(`Will update ${PINNED} in ${join(root, "opencode.json")}`);
    expect(
      plan.some((item) => item.includes(`plugin ${OLD_PLUGIN} → ${CLI_VERSION} via npm`)),
    ).toBe(true);
  });

  test("an entry pinned to the old version is one issue with the --fix remedy", async () => {
    const entry = `${AFT_OPENCODE_PACKAGE}@${OLD_PLUGIN}`;
    const [code, output] = await doctorOutput(entry, OLD_PLUGIN);
    const message = `${OLDER_MESSAGE} The plugin entry ${entry} asks for version ${OLD_PLUGIN}, not the version of this CLI and its binary.`;

    expect(code).toBe(1);
    expectOneIssue(output, message, PIN_AND_UPDATE);
  });

  test("an entry already pinned to this CLI only needs the plugin updated", async () => {
    const [code, output] = await doctorOutput(PINNED, OLD_PLUGIN);

    expect(code).toBe(1);
    expectOneIssue(output, OLDER_MESSAGE, UPDATE_ONLY);
  });

  test("an up-to-date plugin reports no version issue", async () => {
    const [code, output] = await doctorOutput(PINNED, CLI_VERSION);

    expect(code).toBe(0);
    expect(output).not.toContain("is older than CLI");
    expect(output).not.toContain("Remediation:");
    expect(output).not.toContain("to pin it to");
    expect(output).not.toContain("@latest");
    expect(output).toContain(`plugin version: ${CLI_VERSION}`);
  });
});

/**
 * Run `doctor --fix --yes` on the fixture. `npmInstall` stands in for npm: it
 * receives the directory npm would run in and may rewrite what is installed
 * there, the way a real install would.
 */
async function doctorFix(
  entry: string,
  cachedVersion: string,
  npmInstall: (installDir: string) => void = () => {},
  host: OpenCodeHostDetection = V1,
): Promise<{ output: string; npmDirs: string[]; plan: string[]; cachePath: string }> {
  const { adapter, report } = fixture(entry, cachedVersion);
  const plan = buildDoctorFixPlan([adapter], report).map((item) => item.message);
  const cachePath = report.harnesses[0]?.pluginCache.path ?? "";
  const npmDirs: string[] = [];
  const lines = captureOutput();
  await runDoctor({
    clear: false,
    fix: true,
    force: false,
    issue: false,
    argv: ["--fix", "--yes"],
    resolveAdapters: async () => [adapter],
    collectDiagnostics: async () => report,
    collectRemovalHealth: async () => ({ available: false, message: "fixture" }),
    detectOpenCodeHost: () => host,
    applyOnnxFix: async () => null,
    runNative: () => ({
      ok: true,
      stdout: JSON.stringify({ plan_version: 1, features: [] }),
      stderr: "",
      status: 0,
    }),
    runNpmInstall: async (installDir) => {
      npmDirs.push(installDir);
      npmInstall(installDir);
    },
  });
  return { output: stripAnsi(lines.join("\n")), npmDirs, plan, cachePath };
}

describe("doctor --fix: updating an OpenCode plugin older than the CLI", () => {
  test("an entry pinned to an old version is left for OpenCode to install on its next start", async () => {
    const entry = `${AFT_OPENCODE_PACKAGE}@${OLD_PLUGIN}`;
    const { output, npmDirs, plan, cachePath } = await doctorFix(entry, OLD_PLUGIN);

    // npm in the old pin's cache would reinstall the old pin and change nothing.
    expect(npmDirs).toEqual([]);
    expect(output).toContain(`OpenCode will install ${PINNED} on its next start`);
    expect(output).not.toContain("plugin updated");
    // The entry really is re-pinned, which is what makes that message true.
    expect(readFileSync(join(root, "opencode.json"), "utf-8")).toContain(PINNED);
    // The planned-changes line promises what the plugin-update step then did.
    expect(plan).toContainEqual(
      `Will not run npm for OpenCode plugin ${OLD_PLUGIN}: its cache ${cachePath} pins ${OLD_PLUGIN}. With the entry pinned to ${PINNED}, OpenCode will install ${CLI_VERSION} on its next start`,
    );
    expect(plan.some((item) => item.includes("via npm"))).toBe(false);
  });

  test("an old pinned entry that could not be re-pinned is a warning, not an install promise", async () => {
    const entry = `${AFT_OPENCODE_PACKAGE}@${OLD_PLUGIN}`;
    // With both generations on PATH, --fix refuses to write the OpenCode config.
    const ambiguous: OpenCodeHostDetection = {
      status: "ambiguous",
      generations: ["v1", "v2"],
      evidence: V1.evidence,
    };
    const { output, npmDirs } = await doctorFix(entry, OLD_PLUGIN, () => {}, ambiguous);

    expect(npmDirs).toEqual([]);
    expect(output).toContain(
      `OpenCode: plugin not updated; it stays on ${OLD_PLUGIN}. The plugin entry is ${entry}, not ${PINNED}`,
    );
    expect(output).not.toContain("on its next start");
    expect(output).not.toContain("plugin updated");
  });

  test("an @latest entry is updated by npm and reports the version npm installed", async () => {
    const { output, npmDirs, plan, cachePath } = await doctorFix(
      `${AFT_OPENCODE_PACKAGE}@latest`,
      OLD_PLUGIN,
      (installDir) => writeInstalledPlugin(installDir, CLI_VERSION),
    );

    expect(cachePath.endsWith(`${AFT_OPENCODE_PACKAGE}@latest`)).toBe(true);
    expect(npmDirs).toEqual([cachePath]);
    expect(output).toContain(
      `OpenCode: plugin updated ${OLD_PLUGIN} → ${CLI_VERSION} in ${cachePath} (restart OpenCode to apply)`,
    );
    expect(output).not.toContain("on its next start");
    expect(plan).toContainEqual(
      `Will update OpenCode plugin ${OLD_PLUGIN} → ${CLI_VERSION} via npm install in ${cachePath} (the plugin's own auto-update could not run, often no npm on PATH)`,
    );
  });

  test("npm leaving an older version installed is a warning, not an update", async () => {
    const stillOld = "0.0.2";
    const { output, npmDirs, cachePath } = await doctorFix(
      `${AFT_OPENCODE_PACKAGE}@latest`,
      OLD_PLUGIN,
      (installDir) => writeInstalledPlugin(installDir, stillOld),
    );

    expect(npmDirs).toEqual([cachePath]);
    expect(output).toContain(
      `OpenCode: npm install finished in ${cachePath}, but the installed plugin is ${stillOld}, not ${CLI_VERSION}; the plugin was not updated.`,
    );
    expect(output).not.toContain("plugin updated");
    expect(output).not.toContain("plugin package update");
  });
});

describe("doctor never downgrades OpenCode", () => {
  const [major, minor, patch] = CLI_VERSION.split(".").map(Number);
  const newer = `${major}.${minor}.${patch + 1}`;
  const V2: OpenCodeHostDetection = {
    ...V1,
    status: "v2",
    generations: ["v2"],
    evidence: V1.evidence.map((e) => ({ ...e, generation: "v2" })),
  };
  const cases = [
    { host: V1, spec: newer, installed: undefined },
    { host: V1, spec: newer, installed: newer },
    { host: V2, spec: newer, installed: undefined },
    { host: V2, spec: newer, installed: newer },
    { host: V2, spec: "latest", installed: newer },
    { host: V2, spec: "latest", installed: undefined },
    { host: V2, spec: "^0.63.0", installed: undefined },
    { host: V2, spec: `${newer}-beta.2`, installed: `${newer}-beta.2` },
  ];
  for (const { host, spec, installed } of cases) {
    test(`${host.status} ${spec} installed=${installed ?? "missing"}: diagnosis, plan and --fix --yes preserve bytes`, async () => {
      const entry = `${AFT_OPENCODE_PACKAGE}@${spec}`;
      const { adapter, report } = fixture(entry, installed ?? newer);
      rmSync(join(root, "xdg-cache"), { recursive: true, force: true });
      adapter.useHostDetection(host);
      const key = host.status === "v2" ? "plugins" : "plugin";
      const packageEntry =
        host.status === "v2"
          ? { package: entry, options: { enabled: true } }
          : [entry, { enabled: true }];
      const bytes = `${JSON.stringify({ [key]: [packageEntry, "unrelated@1.0.0", ...(host.status === "v2" ? ["-opencode.tool.shell", "-opencode.tool.patch"] : [])], tools: { "-opencode.tool.shell": true, "-opencode.tool.patch": true } }, null, 2)}\n`;
      writeFileSync(join(root, "opencode.json"), bytes);
      if (installed) {
        // Independently construct the host's layout, not the adapter's returned path.
        const path =
          host.status === "v2"
            ? join(root, "xdg-cache/opencode/npm", entry, "1793616638229")
            : join(root, "xdg-cache/opencode/packages", entry);
        writeInstalledPlugin(path, installed);
      }
      const harness = report.harnesses[0]!;
      harness.pluginCache = adapter.getPluginCacheInfo();
      const plan = buildDoctorFixPlan([adapter], report);
      expect(plan.filter((item) => item.kind === "plugin-update")).toEqual([]);
      expect(plan.some((item) => item.message.includes(PINNED))).toBe(false);
      const lines = captureOutput();
      const approved = `This doctor is AFT ${CLI_VERSION}; OpenCode is configured for AFT plugin ${installed ?? spec}. Doctor will not downgrade that entry. Run doctor with a matching or newer CLI.`;
      let npmCalls = 0;
      for (const fix of [false, true]) {
        const outputStart = lines.length;
        await runDoctor({
          clear: false,
          fix,
          force: false,
          issue: false,
          argv: fix ? ["--fix", "--yes"] : [],
          resolveAdapters: async () => [adapter],
          collectDiagnostics: async () => report,
          collectRemovalHealth: async () => ({ available: false, message: "fixture" }),
          detectOpenCodeHost: () => host,
          applyOnnxFix: async () => null,
          runNative: () => ({
            ok: true,
            stdout: JSON.stringify({ plan_version: 1, features: [] }),
            stderr: "",
            status: 0,
          }),
          runNpmInstall: async () => {
            npmCalls++;
          },
        });
        expect(readFileSync(join(root, "opencode.json"), "utf8")).toBe(bytes);
        expect(stripAnsi(lines.slice(outputStart).join("\n"))).toContain(approved);
      }
      expect(npmCalls).toBe(0);
      const output = stripAnsi(lines.join("\n"));
      expect(output).toContain(approved);
      expect(output).not.toContain("to pin it to");
      const markdown = renderDiagnosticsMarkdown(report);
      expect(markdown).toContain(approved);
      const source = installed
        ? "installed"
        : spec === "latest" || spec.startsWith("^")
          ? "unknown"
          : "configured";
      expect(output).toContain(`(${source}`);
      expect(markdown).toContain(`(${source}`);
      expect(markdown).not.toContain("not installed");
    });
  }

  test("SemVer precedence includes prereleases, numeric identifiers and ignores build metadata", () => {
    expect(compareSemver("0.63.5-beta.2", "0.63.4")).toBe(1);
    expect(compareSemver("0.63.5-beta.2", "0.63.5")).toBe(-1);
    expect(compareSemver("0.63.5-beta.10", "0.63.5-beta.2")).toBe(1);
    expect(compareSemver("0.63.4+build.2", "0.63.4+build.1")).toBe(0);
    expect(compareSemver("latest", "0.63.4")).toBeUndefined();
    expect(compareSemver("0.63.5-beta.02", "0.63.4")).toBeUndefined();
  });

  test.skipIf(process.platform === "win32")(
    "CLI binary evidence includes its lookup path and directory-cache meaning",
    async () => {
      const { adapter } = fixture(`${AFT_OPENCODE_PACKAGE}@${newer}`, newer);
      const bin = join(root, "aft-cache", "bin");
      mkdirSync(join(bin, `v${newer}`), { recursive: true });
      expect(getBinaryCacheInfo(CLI_VERSION).activeVersion).toBeNull();
      const cliDir = join(bin, `v${CLI_VERSION}`);
      mkdirSync(cliDir);
      // Cache inventory records the CLI's directory even before an executable exists.
      expect(getBinaryCacheInfo(`v${CLI_VERSION}`).activeVersion).toBe(`v${CLI_VERSION}`);
      const path = join(cliDir, "aft");
      const binaryVersion = `${major}.${minor}.2`;
      writeFileSync(path, `#!/bin/sh\necho 'aft ${binaryVersion}'\n`, { mode: 0o755 });
      const report = await collectDiagnostics([adapter]);
      expect(report.binaryVersion).toBe(binaryVersion);
      expect(report.binaryPath).toBe(path);
      const markdown = renderDiagnosticsMarkdown(report);
      expect(markdown).toContain(
        `CLI lookup of AFT binary: ${binaryVersion} at ${path} (not a probe of the running plugin worker)`,
      );
      expect(markdown).toContain('"CLI version cached":');
      expect(markdown).not.toContain('"activeVersion":');
    },
  );
});
