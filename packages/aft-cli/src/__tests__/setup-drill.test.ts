/// <reference path="../bun-test.d.ts" />

/**
 * Regression tests for a clean install: a config the user cannot
 * write, a machine with no binary yet, download failures, and the feature list
 * layout. Every test runs under a temporary HOME/XDG/AFT_CACHE_DIR so nothing
 * can reach the operator's real config, cache or binary.
 */

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import {
  chmodSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { PassThrough, Writable } from "node:stream";
import { linkCachedExecutable } from "../../../aft-bridge/src/__tests__/test-utils/cached-executable.js";

import { OpenCodeAdapter } from "../adapters/opencode.js";
import type { HarnessAdapter, HarnessConfigPaths } from "../adapters/types.js";
import { runDoctor } from "../commands/doctor.js";
import { runSetup } from "../commands/setup.js";
import type { DiagnosticReport, HarnessDiagnostic } from "../lib/diagnostics.js";
import type { NativeResult, PlanFeature, SetupPlan } from "../lib/feature-plan.js";
import { describePermissionProblem, type PermissionFacts } from "../lib/fs-errors.js";
import { getSelfVersion } from "../lib/self-version.js";
import { type FeatureRow, promptFeatureList, renderRowLines } from "../setup/feature-list.js";
import { runFeatureWizard, type WizardIO } from "../setup/feature-wizard.js";
import type { OpenCodeHostDetection } from "../setup/host-generation.js";

const ENV_KEYS = [
  "HOME",
  "XDG_CONFIG_HOME",
  "XDG_CACHE_HOME",
  "XDG_DATA_HOME",
  "AFT_CACHE_DIR",
  "AFT_STORAGE_DIR",
  "OPENCODE_CONFIG_DIR",
] as const;
const savedEnv: Partial<Record<(typeof ENV_KEYS)[number], string | undefined>> = {};
const originalFetch = globalThis.fetch;
const originalStdoutWrite = process.stdout.write;
const originalStderrWrite = process.stderr.write;
const originalLog = console.log;
const originalError = console.error;
const lockedDirs: string[] = [];
let sandbox = "";

beforeEach(() => {
  sandbox = mkdtempSync(join(tmpdir(), "aft-cli-drill-"));
  for (const key of ENV_KEYS) savedEnv[key] = process.env[key];
  process.env.HOME = join(sandbox, "home");
  process.env.XDG_CONFIG_HOME = join(sandbox, "config");
  process.env.XDG_CACHE_HOME = join(sandbox, "cache");
  process.env.XDG_DATA_HOME = join(sandbox, "data");
  process.env.AFT_CACHE_DIR = join(sandbox, "aft-cache");
  process.env.AFT_STORAGE_DIR = join(sandbox, "storage");
  delete process.env.OPENCODE_CONFIG_DIR;
  mkdirSync(process.env.HOME, { recursive: true });
});

afterEach(() => {
  for (const dir of lockedDirs.splice(0)) chmodSync(dir, 0o755);
  for (const key of ENV_KEYS) {
    if (savedEnv[key] === undefined) delete process.env[key];
    else process.env[key] = savedEnv[key];
  }
  globalThis.fetch = originalFetch;
  process.stdout.write = originalStdoutWrite;
  process.stderr.write = originalStderrWrite;
  console.log = originalLog;
  console.error = originalError;
});

/** Capture everything the command prints, through clack or directly. */
function captureOutput(): string[] {
  const output: string[] = [];
  const capture = ((chunk: string | Uint8Array) => {
    output.push(String(chunk));
    return true;
  }) as typeof process.stdout.write;
  process.stdout.write = capture;
  process.stderr.write = capture as typeof process.stderr.write;
  console.log = (...args: unknown[]) => output.push(args.join(" "));
  console.error = (...args: unknown[]) => output.push(args.join(" "));
  return output;
}

function lockDirectory(dir: string): void {
  mkdirSync(dir, { recursive: true });
  chmodSync(dir, 0o555);
  lockedDirs.push(dir);
}

const runningAsRoot = typeof process.getuid === "function" && process.getuid() === 0;

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

class RootedOpenCodeAdapter extends OpenCodeAdapter {
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
}

function feature(id: string, overrides: Partial<PlanFeature> = {}): PlanFeature {
  const kind = id.startsWith("indexes.")
    ? "index"
    : id.startsWith("github.")
      ? "capability"
      : "tool";
  return {
    id,
    kind,
    group: kind === "index" ? "Indexes" : kind === "capability" ? "GitHub" : "Search/navigation",
    order: 0,
    label: id,
    description: `${id} does a thing.`,
    binding: { path: id, tool_name: kind === "tool" ? id : null },
    default: kind !== "capability",
    configured: kind !== "capability",
    source: "default",
    proposed: kind !== "capability",
    effective: kind === "index" ? "unavailable" : "ready",
    reason: "default",
    available: kind !== "index",
    unavailable_reason: kind === "index" ? "runtime_not_observed" : null,
    cost_note: null,
    prerequisites: [],
    ...overrides,
  };
}

function smallPlan(): SetupPlan {
  const ids = ["aft_outline", "read", "indexes.trigram", "github.read", "github.write"];
  return {
    plan_version: 1,
    features: ids.map((id, index) => feature(id, { order: index + 1 })),
  };
}

function autoIO(seen: string[] = []): WizardIO {
  return {
    selectRows: async (_message, _options, initial) => {
      seen.push("wizard");
      return initial;
    },
    confirm: async (_message, initial) => initial,
    info: () => {},
    note: () => {},
  };
}

describe("B1: a config file the user cannot write", () => {
  test("describes a root-owned tree with its owner and the chown fix", () => {
    const home = "/Users/cortexkit";
    const owners: Record<string, number> = {
      [`${home}/.config/opencode`]: 0,
      [`${home}/.config`]: 0,
      [home]: 501,
    };
    const facts: PermissionFacts = {
      ownerUid: (path) => owners[path] ?? null,
      currentUid: () => 501,
      userName: (uid) => (uid === 0 ? "root" : "cortexkit"),
      exists: (path) => path in owners,
      home: () => home,
    };
    expect(describePermissionProblem(`${home}/.config/opencode/opencode.json`, facts)).toBe(
      "Cannot write ~/.config/opencode/opencode.json: permission denied. ~/.config/opencode is owned by root, not you. Fix: sudo chown -R $(whoami) ~/.config",
    );
  });

  test("setup reports an unwritable config without throwing and still runs the feature step", async () => {
    if (runningAsRoot) return;
    const root = join(sandbox, "opencode-config");
    lockDirectory(root);
    const output = captureOutput();
    const seen: string[] = [];
    const calls: string[][] = [];
    const run = (args: string[]): NativeResult => {
      calls.push(args);
      return args.includes("--plan")
        ? { ok: true, stdout: JSON.stringify(smallPlan()), stderr: "", status: 0 }
        : {
            ok: true,
            stdout: JSON.stringify({ written: join(sandbox, "aft.jsonc") }),
            stderr: "",
            status: 0,
          };
    };

    let code: number | undefined;
    let thrown: unknown = null;
    try {
      code = await runSetup([], {
        resolveAdapters: async () => [new RootedOpenCodeAdapter(root)],
        detectOpenCodeHost: v1,
        features: { run, io: autoIO(seen), interactive: true, checkGh: () => "ready" },
      });
    } catch (error) {
      thrown = error;
    }

    const text = output.join("");
    expect(thrown).toBeNull();
    expect(code).toBe(1);
    expect(text).toContain(`Cannot write ${join(root, "opencode.json")}: permission denied.`);
    expect(text).toContain(`Fix: chmod u+w ${root}`);
    expect(text).not.toContain("    at ");
    // The feature choices do not need opencode.json, so they are still made.
    expect(seen).toEqual(["wizard"]);
    expect(calls.map((args) => args[1])).toEqual(["--plan", "--answers"]);
  });
});

/** A stand-in native binary that serves `setup --plan` and records `setup --answers`. */
function writeFakeBinary(dir: string, plan: SetupPlan): { path: string; answersFile: string } {
  mkdirSync(dir, { recursive: true });
  const planFile = join(dir, "plan.json");
  const answersFile = join(dir, "answers.json");
  writeFileSync(planFile, JSON.stringify(plan));
  const path = join(dir, "aft");
  linkCachedExecutable(
    path,
    [
      "#!/bin/sh",
      'dir=$(dirname "$0")',
      'case "$2" in',
      '  --plan) cat "$dir/plan.json" ;;',
      `  --answers) cat > "$dir/answers.json"; printf '{"written":"%s"}\\n' "$XDG_CONFIG_HOME/cortexkit/aft.jsonc" ;;`,
      "  *) exit 2 ;;",
      "esac",
      "",
    ].join("\n"),
  );
  return { path, answersFile };
}

describe("B2: setup on a machine with no binary", () => {
  test("obtains the matching binary first, then runs the wizard in the same invocation", async () => {
    const root = join(sandbox, "opencode-config");
    mkdirSync(root, { recursive: true });
    const output = captureOutput();
    const events: string[] = [];
    let fake: { path: string; answersFile: string } | null = null;

    const code = await runSetup([], {
      resolveAdapters: async () => [new RootedOpenCodeAdapter(root)],
      detectOpenCodeHost: v1,
      findBinary: () => null,
      downloadBinary: async (tag) => {
        events.push(`download ${tag}`);
        fake = writeFakeBinary(join(sandbox, "downloaded"), smallPlan());
        return fake.path;
      },
      features: { io: autoIO(events), interactive: true, checkGh: () => "ready" },
    });

    const text = output.join("");
    expect(events).toEqual([`download v${getSelfVersion()}`, "wizard"]);
    expect(code).toBe(0);
    expect(fake).not.toBeNull();
    const answers = JSON.parse(
      readFileSync((fake as unknown as { answersFile: string }).answersFile, "utf8"),
    );
    expect(answers.selections.aft_outline).toBe(true);
    // The save line names the file the binary reported writing.
    expect(text).toContain(
      `Saved feature choices to ${join(process.env.XDG_CONFIG_HOME as string, "cortexkit", "aft.jsonc")}.`,
    );
    // The restart note comes after the choices are saved, not before.
    expect(text.indexOf("Next steps")).toBeGreaterThan(text.indexOf("Saved feature choices"));
  });

  test("when the binary cannot be obtained, names the cause and the npx retry command", async () => {
    const root = join(sandbox, "opencode-config");
    mkdirSync(root, { recursive: true });
    const output = captureOutput();

    const code = await runSetup([], {
      resolveAdapters: async () => [new RootedOpenCodeAdapter(root)],
      detectOpenCodeHost: v1,
      findBinary: () => null,
      downloadBinary: async () => {
        throw new TypeError("fetch failed");
      },
      features: { io: autoIO(), interactive: true, checkGh: () => "ready" },
    });

    const text = output.join("");
    expect(code).toBe(1);
    expect(text).toContain("could not reach GitHub");
    expect(text).toContain("`npx @cortexkit/aft doctor --fix`");
    expect(text).not.toMatch(/(^|[^/])`aft doctor/m);
  });
});

function doctorFixture(root: string): { adapter: HarnessAdapter; report: DiagnosticReport } {
  const configPaths: HarnessConfigPaths = {
    configDir: root,
    harnessConfig: join(root, "opencode.json"),
    harnessConfigFormat: "json",
    aftConfig: join(root, "aft.jsonc"),
    aftConfigFormat: "none",
  };
  mkdirSync(root, { recursive: true });
  writeFileSync(
    configPaths.harnessConfig,
    JSON.stringify({ plugin: ["@cortexkit/aft-opencode@latest"] }),
  );
  const adapter: HarnessAdapter = {
    kind: "opencode",
    displayName: "OpenCode",
    pluginPackageName: "@cortexkit/aft-opencode",
    pluginEntryWithVersion: "@cortexkit/aft-opencode@latest",
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
    storageDir: { path: join(root, "storage"), exists: true, accessible: true, sizesByKey: {} },
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
    report: {
      timestamp: new Date(0).toISOString(),
      platform: process.platform,
      arch: process.arch,
      nodeVersion: process.version,
      cliVersion: getSelfVersion(),
      binaryVersion: null,
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

/** What the CLI's native runner returns when no binary is installed. */
const noBinary = (): NativeResult => ({
  ok: false,
  stdout: "",
  stderr: "the AFT binary is not installed; run `npx @cortexkit/aft doctor --fix` to install it",
  status: null,
  missingBinary: true,
});

/**
 * Run `doctor --fix --yes` with the real downloader (aft-bridge's
 * ensureBinary) against a stubbed network, and return what it printed.
 */
async function doctorFixOutput(): Promise<string> {
  const fixture = doctorFixture(join(sandbox, "doctor"));
  const output = captureOutput();
  await runDoctor({
    clear: false,
    fix: true,
    force: false,
    issue: false,
    argv: ["--fix", "--yes"],
    resolveAdapters: async () => [fixture.adapter],
    collectDiagnostics: async () => fixture.report,
    detectOpenCodeHost: v1,
    runNative: noBinary,
  });
  return output.join("");
}

function stubFetch(respond: (url: string, binaryUrl: string | null) => Response): void {
  let binaryUrl: string | null = null;
  globalThis.fetch = (async (input: string | URL | Request) => {
    const url = String(input instanceof Request ? input.url : input);
    if (!url.endsWith("checksums.sha256")) binaryUrl = url;
    return respond(url, binaryUrl);
  }) as typeof fetch;
}

function downloadFailureLine(text: string): string {
  return text.split("\n").find((line) => line.includes("AFT binary download failed")) ?? "";
}

describe("B3: doctor --fix names the real download failure", () => {
  test("permission: an unwritable binary cache names the directory and the fix", async () => {
    if (runningAsRoot) return;
    lockDirectory(process.env.AFT_CACHE_DIR as string);
    stubFetch(() => new Response("unreachable", { status: 500 }));
    const line = downloadFailureLine(await doctorFixOutput());
    expect(line).toContain("permission denied");
    expect(line).toContain(`Fix: chmod u+w ${process.env.AFT_CACHE_DIR}`);
    expect(line).not.toContain("no matching release asset");
  });

  test("network: an unreachable GitHub is reported as a network failure", async () => {
    globalThis.fetch = (async () => {
      throw new TypeError("fetch failed");
    }) as unknown as typeof fetch;
    const line = downloadFailureLine(await doctorFixOutput());
    expect(line).toContain("could not reach GitHub");
    expect(line).not.toContain("no matching release asset");
  });

  test("missing asset: a 404 says the release has no binary for this platform", async () => {
    stubFetch(() => new Response("Not Found", { status: 404, statusText: "Not Found" }));
    const line = downloadFailureLine(await doctorFixOutput());
    expect(line).toContain("has no binary for");
    expect(line).toContain("HTTP 404");
  });

  test("checksum: a mismatched download is reported as a checksum failure", async () => {
    stubFetch((url, binaryUrl) =>
      url.endsWith("checksums.sha256")
        ? new Response(`${"0".repeat(64)}  ${(binaryUrl ?? "").split("/").at(-1)}\n`)
        : new Response("not really a binary"),
    );
    const line = downloadFailureLine(await doctorFixOutput());
    expect(line).toContain("failed checksum verification");
    expect(line).not.toContain("no matching release asset");
  });

  test("no raw bridge log lines, no migration failure, no advice to rerun doctor --fix", async () => {
    stubFetch(() => new Response("Not Found", { status: 404, statusText: "Not Found" }));
    const text = await doctorFixOutput();
    expect(text).not.toContain("[aft-bridge]");
    expect(text).not.toContain("config migration failed");
    const errorLines = text.split("\n").filter((line) => line.includes("■"));
    for (const line of errorLines) expect(line).not.toContain("doctor --fix");
  });
});

/** Every row description from the native catalog that is longer than 60 columns. */
const LONG_DESCRIPTIONS: Record<string, string> = {
  grep: "AFT takes over the host grep tool; indexed when the trigram index is ready, filesystem matching otherwise.",
  aft_zoom: "Read one symbol or section; optional call-graph annotations use the callgraph index.",
  "indexes.semantic":
    "Background embedding index for meaning-based search (the semantic aft_search lane).",
  "indexes.callgraph":
    "Persisted call graph used by aft_callgraph, zoom annotations, dead-code hints and search enrichment.",
};

function renderPlan(): SetupPlan {
  const rows: [string, string, string][] = [
    ["grep", "Search/navigation", "grep"],
    ["aft_zoom", "Search/navigation", "aft_zoom"],
    ["read", "Editing", "read"],
    ["indexes.semantic", "Indexes", "Semantic index"],
    ["indexes.callgraph", "Indexes", "Callgraph index"],
  ];
  return {
    plan_version: 1,
    features: rows.map(([id, group, label], index) =>
      feature(id, {
        group,
        label,
        order: index + 1,
        description: LONG_DESCRIPTIONS[id] ?? "AFT takes over the host read tool.",
      }),
    ),
  };
}

/** Render the live prompt's first frame at `columns`, as plain text. */
async function firstFrame(columns: number): Promise<string> {
  let frame = "";
  const output = new Writable({
    write(chunk, _encoding, callback) {
      frame += chunk.toString();
      callback();
    },
  }) as Writable & { columns: number; rows: number; isTTY: boolean };
  output.columns = columns;
  output.rows = 60;
  output.isTTY = true;
  const input = new PassThrough() as PassThrough & { isTTY: boolean; setRawMode: () => void };
  input.isTTY = true;
  input.setRawMode = () => {};
  // Take the rows exactly as the wizard hands them to the prompt, so the
  // frame shows what setup would put on screen.
  let rows: Record<string, FeatureRow[]> = {};
  let initial: string[] = [];
  await runFeatureWizard(
    renderPlan(),
    {
      selectRows: async (_message, options, selected) => {
        rows = options;
        initial = selected.filter((id) => id !== "read");
        return selected;
      },
      confirm: async () => false,
      info: () => {},
      note: () => {},
    },
    () => "ready",
  );
  // Setup rows carry no runtime state: the plan says every index is
  // "unavailable: runtime_not_observed" and every tool "ready".
  expect(JSON.stringify(rows)).not.toMatch(/\(now |runtime_not_observed|unavailable/);
  const done = promptFeatureList(
    "Choose the AFT features to enable",
    rows,
    initial,
    () => {
      throw new Error("cancelled");
    },
    { input, output },
  ).catch(() => []);
  input.write("\x03");
  await done;
  // biome-ignore lint/suspicious/noControlCharactersInRegex: strip ANSI escapes
  const plain = frame.replace(/\x1b\[[0-9;?]*[A-Za-z]/g, "");
  return plain.slice(0, plain.indexOf("Enter: confirm") + "Enter: confirm".length);
}

describe("the feature list cursor", () => {
  // Unselected rows keep their descriptions while the cursor moves (checked
  // in a real 80x24 PTY); what moves is the one full-brightness label.
  test("only the focused row's label is at full brightness, checked or not", () => {
    const row = { value: "grep", label: "grep", description: "Fast search." };
    // styleText drops styling when stdout is not a terminal; force it on.
    const savedForceColor = process.env.FORCE_COLOR;
    process.env.FORCE_COLOR = "1";
    const label = (state: Parameters<typeof renderRowLines>[1]) =>
      renderRowLines(row, state, false, 80)[0] as string;
    const dimmed = "\u001b[2mgrep";
    try {
      expect(label("active-selected")).not.toContain(dimmed);
      expect(label("active")).not.toContain(dimmed);
      expect(label("selected")).toContain(dimmed);
      expect(label("inactive")).toContain(dimmed);
    } finally {
      if (savedForceColor === undefined) delete process.env.FORCE_COLOR;
      else process.env.FORCE_COLOR = savedForceColor;
    }
    for (const state of ["active", "selected", "active-selected", "inactive"] as const) {
      expect(renderRowLines(row, state, false, 80)).toHaveLength(2);
    }
  });
});

describe("U6-U8: the feature list at 80 and 120 columns", () => {
  test("80 columns: name and description only, wrapped inside the tree", async () => {
    const frame = await firstFrame(80);
    expect(frame).toBe(
      [
        "│",
        "◆  Choose the AFT features to enable",
        "│  ◼ Search/navigation",
        "│  │ ◼ grep",
        "│  │   AFT takes over the host grep tool; indexed when the trigram index is",
        "│  │   ready, filesystem matching otherwise.",
        "│  └ ◼ aft_zoom",
        "│      Read one symbol or section; optional call-graph annotations use the",
        "│      callgraph index.",
        "│  ◻ Editing",
        "│  └ ◻ read",
        "│      AFT takes over the host read tool.",
        "│  ◼ Indexes",
        "│  │ ◼ Semantic index",
        "│  │   Background embedding index for meaning-based search (the semantic",
        "│  │   aft_search lane).",
        "│  └ ◼ Callgraph index",
        "│      Persisted call graph used by aft_callgraph, zoom annotations, dead-code",
        "│      hints and search enrichment.",
        "│  ↑/↓ to navigate • Space: select • Enter: confirm",
      ].join("\n"),
    );
  });

  test("no runtime status and an unbroken tree at 80 and 120 columns", async () => {
    for (const columns of [80, 120]) {
      const frame = await firstFrame(columns);
      expect(frame).not.toMatch(/\(now |runtime_not_observed|unavailable|ready\)/);
      const body = frame.split("\n").slice(2, -1);
      for (const line of body) {
        expect(line.length).toBeLessThanOrEqual(columns);
        // Every body line keeps the guide bar and a tree column: a group
        // header, a branch, or a description indented under its branch.
        expect(line).toMatch(/^│ {2}(◻ |◼ |│ |└ | {4})/);
      }
    }
  });
});
