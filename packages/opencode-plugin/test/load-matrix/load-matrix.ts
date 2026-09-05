// Every spawned host receives HOME and XDG directories under this suite's temp root.
// Otherwise the V2 client can discover the operator's OpenCode service and modify its
// database or logs. Normal runs hash those operator files before and after the matrix and
// compare metadata around each invocation. AFT_LOAD_MATRIX_ALLOW_LIVE_OPERATOR is only for
// local runs beside an active operator process; it permits database content and mtime
// changes from that process while still requiring stable database size and unchanged logs.
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { type ChildProcess, spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { createReadStream, existsSync } from "node:fs";
import { cp, mkdir, mkdtemp, readdir, readFile, rm, stat, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, join, relative } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import ts from "typescript";

import {
  prepareSubcLane,
  type SubcRig,
  startSubcRig,
} from "../../../aft-bridge/src/__tests__/e2e/subc-rig.js";

const pluginRoot = join(dirname(fileURLToPath(import.meta.url)), "../..");
const repoRoot = join(pluginRoot, "../..");
const packageName = "@cortexkit/aft-opencode";
const v2Version = "0.0.0-beta-19059";
const allowLiveOperatorWrites = process.env.AFT_LOAD_MATRIX_ALLOW_LIVE_OPERATOR === "1";
const suiteTempParent = join(pluginRoot, "tmp");
const operatorDb = join(homedir(), ".local", "share", "opencode", "opencode.db");
const operatorLogDir = join(homedir(), ".local", "share", "opencode", "log");

type CommandResult = {
  status: number | null;
  stdout: string;
  stderr: string;
};

type HostInstalls = {
  v1: string;
  v2: string;
  v1Output: string;
  v2Output: string;
};

type PathSnapshot = {
  path: string;
  exists: boolean;
  size?: number;
  mtimeMs?: number;
  sha256?: string;
};

type HostIsolation = {
  root: string;
  project: string;
  env: NodeJS.ProcessEnv;
};

let tempRoot = "";
let packedRoot = "";
let tarball = "";
let modernV1 = "";
let hostInstalls: Promise<HostInstalls> | undefined;
let operatorBefore: { db: PathSnapshot; logs: PathSnapshot };
const hostCanaries: Array<{ label: string; before: unknown; after: unknown }> = [];
let subcRig: SubcRig | undefined;

function run(
  command: string,
  args: string[],
  cwd: string,
  options: { env?: NodeJS.ProcessEnv; allowFailure?: boolean } = {},
): CommandResult {
  const result = spawnSync(command, args, {
    cwd,
    encoding: "utf8",
    env: options.env ?? process.env,
    stdio: ["ignore", "pipe", "pipe"],
    maxBuffer: 50 * 1024 * 1024,
    shell: process.platform === "win32",
  });
  if (result.status !== 0 && !options.allowFailure) {
    throw new Error(
      `${command} ${args.join(" ")} failed (${result.status ?? "unknown"})\n${result.stdout}\n${result.stderr}`,
    );
  }
  return { status: result.status, stdout: result.stdout, stderr: result.stderr };
}

function packedFilename(stdout: string): string {
  const parsed = JSON.parse(stdout.trim()) as Array<{ filename?: string }>;
  const filename = parsed[0]?.filename;
  if (!filename) throw new Error(`npm pack did not report a filename: ${stdout}`);
  return filename;
}

function defaultObjectKeys(source: string, fileName: string): string[] {
  const sourceFile = ts.createSourceFile(fileName, source, ts.ScriptTarget.Latest, true);
  let object: ts.ObjectLiteralExpression | undefined;
  for (const statement of sourceFile.statements) {
    if (ts.isExportAssignment(statement) && ts.isObjectLiteralExpression(statement.expression)) {
      object = statement.expression;
      break;
    }
  }
  if (!object) throw new Error(`${fileName} has no default object literal`);
  return object.properties
    .map((property) => property.name)
    .filter((name): name is ts.PropertyName => name !== undefined)
    .map((name) => name.getText(sourceFile).replaceAll(/["']/g, ""))
    .sort();
}

function moduleExportNames(source: string, fileName: string): string[] {
  const sourceFile = ts.createSourceFile(fileName, source, ts.ScriptTarget.Latest, true);
  const names: string[] = [];
  for (const statement of sourceFile.statements) {
    if (ts.isExportAssignment(statement)) names.push("default");
    if (!ts.isExportDeclaration(statement)) continue;
    if (!statement.exportClause || !ts.isNamedExports(statement.exportClause)) {
      names.push("*");
      continue;
    }
    for (const element of statement.exportClause.elements) names.push(element.name.text);
  }
  return names.sort();
}

async function hashFile(hash: ReturnType<typeof createHash>, path: string): Promise<void> {
  for await (const chunk of createReadStream(path)) hash.update(chunk);
}

async function snapshotPath(path: string, includeBytes = true): Promise<PathSnapshot> {
  try {
    const info = await stat(path);
    let sha256: string | undefined;
    if (includeBytes) {
      const hash = createHash("sha256");
      if (info.isDirectory()) {
        const files = (await fixtureFiles(path)).sort();
        for (const file of files) {
          hash.update(relative(path, file));
          hash.update("\0");
          await hashFile(hash, file);
          hash.update("\0");
        }
      } else {
        await hashFile(hash, path);
      }
      sha256 = hash.digest("hex");
    }
    return {
      path,
      exists: true,
      size: info.size,
      mtimeMs: info.mtimeMs,
      ...(sha256 ? { sha256 } : {}),
    };
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return { path, exists: false };
    throw error;
  }
}

async function snapshotOperatorState(
  includeBytes = true,
): Promise<{ db: PathSnapshot; logs: PathSnapshot }> {
  return {
    db: await snapshotPath(operatorDb, includeBytes),
    logs: await snapshotPath(operatorLogDir, includeBytes),
  };
}

async function withOperatorCanary<T>(label: string, operation: () => T | Promise<T>): Promise<T> {
  const before = await snapshotOperatorState(false);
  const result = await operation();
  const after = await snapshotOperatorState(false);
  hostCanaries.push({ label, before, after });
  if (allowLiveOperatorWrites) {
    expect(after.db.size).toBe(before.db.size);
    expect(after.logs).toEqual(before.logs);
  } else {
    expect(after).toEqual(before);
  }
  return result;
}

async function installHost(
  name: string,
  pluginVersion: string,
  dependencies: Record<string, string>,
): Promise<{ root: string; output: string }> {
  const root = join(tempRoot, name);
  await mkdir(root, { recursive: true });
  await writeFile(
    join(root, "package.json"),
    `${JSON.stringify(
      {
        private: true,
        type: "module",
        dependencies: {
          [packageName]: `file:${tarball}`,
          "@opencode-ai/plugin": pluginVersion,
          ...dependencies,
        },
      },
      null,
      2,
    )}\n`,
  );
  const result = run("npm", ["install", "--no-audit", "--no-fund"], root);
  return { root, output: `${result.stdout}\n${result.stderr}` };
}

async function ensureHostInstalls(): Promise<HostInstalls> {
  hostInstalls ??= (async () => {
    const v1 = await installHost("host-v1", modernV1, { "opencode-ai": modernV1 });
    const v2 = await installHost("host-v2", v2Version, {
      "@opencode-ai/cli": v2Version,
      "@opencode-ai/core": v2Version,
    });
    const peerWarning = /ERESOLVE|overrid(?:e|ing).*peer|peer dependency|peer dep missing/i;
    expect(v1.output).not.toMatch(peerWarning);
    expect(v2.output).not.toMatch(peerWarning);
    return { v1: v1.root, v2: v2.root, v1Output: v1.output, v2Output: v2.output };
  })();
  return hostInstalls;
}

async function fixtureFiles(root: string): Promise<string[]> {
  const files: string[] = [];
  for (const entry of await readdir(root, { withFileTypes: true })) {
    const path = join(root, entry.name);
    if (entry.isDirectory()) files.push(...(await fixtureFiles(path)));
    else if (entry.isFile()) files.push(path);
  }
  return files;
}

async function makeIsolation(label: string): Promise<HostIsolation> {
  const root = join(tempRoot, "isolated-hosts", label);
  const project = join(root, "project");
  const home = join(root, "home");
  const config = join(root, "config");
  const data = join(root, "data");
  const state = join(root, "state");
  const cache = join(root, "cache");
  const temp = join(root, "tmp");
  await Promise.all(
    [project, home, config, data, state, cache, temp].map((path) =>
      mkdir(path, { recursive: true }),
    ),
  );
  run("git", ["init", "-q"], project);
  const env: NodeJS.ProcessEnv = {
    ...process.env,
    HOME: home,
    XDG_CONFIG_HOME: config,
    XDG_DATA_HOME: data,
    XDG_STATE_HOME: state,
    XDG_CACHE_HOME: cache,
    TMPDIR: temp,
    NODE_ENV: "test",
    AFT_LOG_STDERR: "1",
    OPENCODE_DISABLE_DEFAULT_PLUGINS: "true",
  };
  delete env.OPENCODE_CONFIG;
  delete env.OPENCODE_CONFIG_CONTENT;
  delete env.OPENCODE_SERVER;
  return { root, project, env };
}

async function copyInstalledPlugin(hostRoot: string, label: string): Promise<string> {
  const source = join(hostRoot, "node_modules", "@cortexkit", "aft-opencode");
  const destination = join(hostRoot, "node_modules", ".load-matrix-packages", label);
  await cp(source, destination, { recursive: true });
  return destination;
}

async function writeAftConfig(
  isolation: HostIsolation,
  aftConfig: Record<string, unknown>,
): Promise<void> {
  const projectConfigDir = join(isolation.project, ".cortexkit");
  await mkdir(projectConfigDir, { recursive: true });
  await writeFile(join(projectConfigDir, "aft.jsonc"), `${JSON.stringify(aftConfig, null, 2)}\n`);
}

async function writeV1Configs(
  isolation: HostIsolation,
  pluginRoots: string[],
  aftConfig: Record<string, unknown>,
  tui = false,
): Promise<void> {
  const configDir = join(isolation.env.XDG_CONFIG_HOME ?? "", "opencode");
  await mkdir(configDir, { recursive: true });
  await writeFile(
    join(configDir, tui ? "tui.json" : "opencode.json"),
    `${JSON.stringify({ plugin: pluginRoots.map((root) => pathToFileURL(root).href) }, null, 2)}\n`,
  );
  await writeAftConfig(isolation, aftConfig);
}

function v1Binary(hostRoot: string): string {
  return join(
    hostRoot,
    "node_modules",
    ".bin",
    process.platform === "win32" ? "opencode.cmd" : "opencode",
  );
}

function runV1ConfigHost(hostRoot: string, isolation: HostIsolation): CommandResult {
  return run(
    v1Binary(hostRoot),
    ["--print-logs", "--log-level", "DEBUG", "debug", "config"],
    isolation.project,
    { env: isolation.env },
  );
}

async function runUntilMarker(
  executable: string,
  args: string[],
  cwd: string,
  env: NodeJS.ProcessEnv,
  marker: string,
  timeoutMs: number,
): Promise<CommandResult> {
  let stdout = "";
  let stderr = "";
  const child: ChildProcess = spawn(executable, args, {
    cwd,
    env,
    stdio: ["ignore", "pipe", "pipe"],
    shell: process.platform === "win32",
  });
  child.stdout?.on("data", (chunk) => {
    stdout += String(chunk);
  });
  child.stderr?.on("data", (chunk) => {
    stderr += String(chunk);
  });

  const deadline = Date.now() + timeoutMs;
  while (!existsSync(marker) && child.exitCode === null && Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  child.kill("SIGTERM");
  await new Promise<void>((resolve) => {
    if (child.exitCode !== null) return resolve();
    child.once("exit", () => resolve());
    setTimeout(() => {
      child.kill("SIGKILL");
      resolve();
    }, 2_000).unref();
  });
  return { status: child.exitCode, stdout, stderr };
}

async function writeV2CoreProbe(hostRoot: string, mode: "load" | "reject"): Promise<string> {
  const probe = join(hostRoot, `core-loader-${mode}.mjs`);
  await writeFile(
    probe,
    `
import { Effect } from "effect";
import { load } from "@opencode-ai/core/plugin/module";
import { Host } from "@opencode-ai/plugin/host";
import { Npm } from "@opencode-ai/util/npm";

const packageRoot = process.argv[2];
const installed = {
  directory: packageRoot,
  name: ${JSON.stringify(packageName)},
  version: ${JSON.stringify(v2Version)},
  revision: "load-matrix",
};
const npm = {
  add: () => Effect.succeed(installed),
  resolve: () => Effect.succeed(installed),
  check: () => Effect.succeed(true),
  update: () => Effect.succeed(installed),
  which: () => Effect.succeed(undefined),
};
const operation = { type: "add", target: ${JSON.stringify(packageName)}, options: {} };
const program = load(operation, { install: false }).pipe(Effect.provideService(Npm.Service, npm));
${
  mode === "load"
    ? `const loaded = await Effect.runPromise(program);
if (loaded.pending) throw new Error("host loader returned pending");
const entrypoints = Host.resolve(installed);
const directory = process.argv[3];
const context = {
  location: {
    directory,
    project: { id: "load-matrix", directory, canonical: directory },
  },
};
await Effect.runPromise(Effect.scoped(loaded.effect(context)));
console.log("[load-matrix-host:v2] resolvedEntry=" + entrypoints.server + " selected=effect");`
    : `try {
  await Effect.runPromise(program);
  console.error("host loader unexpectedly accepted function default");
  process.exitCode = 2;
} catch (error) {
  console.log("[load-matrix-host:v2-negative] " + String(error));
}`
}
`,
  );
  return probe;
}

beforeAll(async () => {
  modernV1 = (await readFile(join(repoRoot, ".github/opencode-version.txt"), "utf8")).trim();
  if (!modernV1) throw new Error(".github/opencode-version.txt is empty");

  await mkdir(suiteTempParent, { recursive: true });
  tempRoot = await mkdtemp(join(suiteTempParent, "load-matrix-"));
  run("bun", ["run", "build"], join(repoRoot, "packages/aft-bridge"));
  run("bun", ["run", "build"], pluginRoot);
  const packed = run("npm", ["pack", "--json", "--pack-destination", tempRoot], pluginRoot);
  tarball = join(tempRoot, packedFilename(packed.stdout));
  packedRoot = join(tempRoot, "packed", "package");
  await mkdir(dirname(packedRoot), { recursive: true });
  run("tar", ["-xzf", tarball, "-C", dirname(packedRoot)], tempRoot);
  await ensureHostInstalls();
  operatorBefore = await snapshotOperatorState(!allowLiveOperatorWrites);
}, 300_000);

afterAll(async () => {
  await subcRig?.cleanup();
  if (tempRoot) await rm(tempRoot, { recursive: true, force: true });
}, 30_000);

describe("packed module shapes", () => {
  test("root, server, and tui expose the governed shapes", async () => {
    const manifest = JSON.parse(await readFile(join(packedRoot, "package.json"), "utf8"));
    const rootSource = await readFile(join(packedRoot, manifest.exports["."].import), "utf8");
    const serverBuiltSource = await readFile(
      join(packedRoot, manifest.exports["./server"].import),
      "utf8",
    );
    const serverSource = await readFile(join(packedRoot, "src/entry/server.mjs"), "utf8");
    const tuiSource = await readFile(join(packedRoot, "src/entry/tui.mjs"), "utf8");

    expect(moduleExportNames(rootSource, "dist/index.js")).toEqual(["default"]);
    expect(moduleExportNames(serverBuiltSource, "dist/entry/server.js")).toEqual(["default"]);
    expect(moduleExportNames(serverSource, "src/entry/server.mjs")).toEqual(["default"]);
    expect(moduleExportNames(tuiSource, "src/entry/tui.mjs")).toEqual(["default"]);
    expect(defaultObjectKeys(serverSource, "src/entry/server.mjs")).toEqual([
      "effect",
      "id",
      "server",
    ]);
    expect(defaultObjectKeys(tuiSource, "src/entry/tui.mjs")).toEqual(["id", "setup", "tui"]);
  });

  test("packed manifest retains discovery and all public subpaths", async () => {
    const manifest = JSON.parse(await readFile(join(packedRoot, "package.json"), "utf8"));
    expect(manifest["oc-plugin"]).toEqual(["server", "tui"]);
    expect(Object.keys(manifest.exports).sort()).toEqual([".", "./server", "./tui"]);
    expect(manifest.dependencies.effect).toBe("4.0.0-rc.112");
    expect(manifest.peerDependencies["@opencode-ai/plugin"]).toBe(">=0.0.0-beta-0");
  });

  test("operator canary detects same-size byte changes", async () => {
    const canary = join(tempRoot, "canary-proof.txt");
    await writeFile(canary, "before");
    const before = await snapshotPath(canary);
    await writeFile(canary, "after!");
    const after = await snapshotPath(canary);

    expect(before.size).toBe(after.size);
    expect(before.sha256).not.toBe(after.sha256);
  });

  test("modern V1 version is sourced only from the repository pin", async () => {
    expect(modernV1).toMatch(/^1\.18\./);
    const files = await fixtureFiles(join(pluginRoot, "test"));
    const literals: string[] = [];
    for (const path of files) {
      const text = await readFile(path, "utf8");
      if (/\b1\.18\.\d+\b/.test(text)) literals.push(path);
    }
    expect(literals).toEqual([]);
  });
});

describe("real host load matrix", () => {
  test("packed installs have no peer warning under modern V1 and the beta host", async () => {
    const installs = await ensureHostInstalls();
    expect(installs.v1Output).not.toContain("ERESOLVE");
    expect(installs.v2Output).not.toContain("ERESOLVE");
    expect(existsSync(v1Binary(installs.v1))).toBe(true);
    expect(
      existsSync(
        join(installs.v2, "node_modules", "@opencode-ai", "core", "dist", "plugin", "module.js"),
      ),
    ).toBe(true);
  }, 240_000);

  test("modern V1 host selects ./server and ignores effect and setup", async () => {
    const { v1 } = await ensureHostInstalls();
    const isolation = await makeIsolation("v1-server");
    const packageRoot = await copyInstalledPlugin(v1, "v1-server");
    const marker = join(isolation.root, "entry.log");
    const manifestPath = join(packageRoot, "package.json");
    const manifest = JSON.parse(await readFile(manifestPath, "utf8"));
    manifest.exports["./server"].import = "./load-matrix-server.mjs";
    await writeFile(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);
    await writeFile(
      join(packageRoot, "load-matrix-server.mjs"),
      `
import { appendFileSync } from "node:fs";
import original from "./dist/entry/server.js";
appendFileSync(process.env.AFT_LOAD_MATRIX_MARKER, "resolvedEntry=./server\\n");
console.error("[load-matrix-host:v1] resolvedEntry=./server");
export default {
  ...original,
  effect: (...args) => {
    appendFileSync(process.env.AFT_LOAD_MATRIX_MARKER, "effect-called\\n");
    return original.effect(...args);
  },
  setup: async () => appendFileSync(process.env.AFT_LOAD_MATRIX_MARKER, "setup-called\\n"),
};
`,
    );
    await writeFile(
      join(packageRoot, "dist", "index.js"),
      `throw new Error("ROOT_ENTRY_SELECTED");\nexport default async function rootTrap() {}\n`,
    );
    isolation.env.AFT_LOAD_MATRIX_MARKER = marker;
    await writeV1Configs(isolation, [packageRoot], { enabled: false });

    const result = await withOperatorCanary("v1-server", () => runV1ConfigHost(v1, isolation));
    const transcript = `${result.stdout}\n${result.stderr}`;
    const events = await readFile(marker, "utf8");
    console.log(
      `[v1-host-transcript]\n${transcript
        .split(/\r?\n/)
        .filter((line) => line.includes("load-matrix-host") || line.includes("AFT disabled"))
        .join("\n")}`,
    );
    expect(transcript).toContain("AFT disabled by config");
    expect(transcript).not.toContain("ROOT_ENTRY_SELECTED");
    expect(events).toBe("resolvedEntry=./server\n");
  }, 120_000);

  test("modern V1 host loads ./tui while ignoring its setup key", async () => {
    const { v1 } = await ensureHostInstalls();
    const isolation = await makeIsolation("v1-tui");
    const packageRoot = await copyInstalledPlugin(v1, "v1-tui");
    const marker = join(isolation.root, "tui-entry.log");
    const manifestPath = join(packageRoot, "package.json");
    const manifest = JSON.parse(await readFile(manifestPath, "utf8"));
    manifest.exports["./tui"].import = "./load-matrix-tui.mjs";
    await writeFile(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);
    await writeFile(
      join(packageRoot, "load-matrix-tui.mjs"),
      `
import { appendFileSync } from "node:fs";
import original from "./src/entry/tui.mjs";
export default {
  ...original,
  tui: async (...args) => {
    appendFileSync(process.env.AFT_LOAD_MATRIX_MARKER, "resolvedEntry=./tui\\ntui-called\\n");
    console.error("[load-matrix-host:v1-tui] resolvedEntry=./tui selected=tui");
    return original.tui(...args);
  },
  setup: async () => appendFileSync(process.env.AFT_LOAD_MATRIX_MARKER, "setup-called\\n"),
};
`,
    );
    isolation.env.AFT_LOAD_MATRIX_MARKER = marker;
    await writeV1Configs(isolation, [packageRoot], { enabled: false }, true);

    await withOperatorCanary("v1-tui", () =>
      runUntilMarker(v1Binary(v1), [], isolation.project, isolation.env, marker, 30_000),
    );
    const events = existsSync(marker) ? await readFile(marker, "utf8") : "";
    console.log(`[v1-tui-host-transcript]\n${events.trim()}`);
    expect(events).toContain("resolvedEntry=./tui\n");
    expect(events).toContain("tui-called\n");
    expect(events).not.toContain("setup-called");
  }, 60_000);

  for (const runtime of ["bun", "node"] as const) {
    test(`V2 ${runtime} host loader selects effect before setup and runs it once`, async () => {
      const { v2 } = await ensureHostInstalls();
      const packageRoot = await copyInstalledPlugin(v2, `v2-${runtime}`);
      const isolation = await makeIsolation(`v2-${runtime}`);
      const marker = join(isolation.root, "entry.log");
      await writeAftConfig(isolation, { enabled: false });
      const manifestPath = join(packageRoot, "package.json");
      const manifest = JSON.parse(await readFile(manifestPath, "utf8"));
      manifest.exports["./server"].import = "./load-matrix-server.mjs";
      await writeFile(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);
      await writeFile(
        join(packageRoot, "load-matrix-server.mjs"),
        `
import { appendFileSync } from "node:fs";
import { Effect } from "effect";
import original from "./dist/entry/server.js";
const effect = (context) => Effect.gen(function* () {
  yield* Effect.sync(() => appendFileSync(process.env.AFT_LOAD_MATRIX_MARKER, "effect-called\\n"));
  yield* original.effect(context);
});
const setup = async () => appendFileSync(process.env.AFT_LOAD_MATRIX_MARKER, "setup-called\\n");
export default { id: original.id, effect, setup };
`,
      );
      const probe = await writeV2CoreProbe(v2, "load");
      const result = await withOperatorCanary(`v2-${runtime}`, () =>
        run(runtime, [probe, packageRoot, isolation.project], v2, {
          env: { ...isolation.env, AFT_LOAD_MATRIX_MARKER: marker },
        }),
      );
      const transcript = `${result.stdout}\n${result.stderr}`;
      console.log(`[v2-${runtime}-host-transcript]\n${transcript}`);
      expect(transcript).toContain("[load-matrix-host:v2]");
      expect(transcript).toContain("load-matrix-server.mjs");
      expect(await readFile(marker, "utf8")).toBe("effect-called\n");
      const resolveArgs =
        runtime === "node"
          ? ["--input-type=module", "-e", 'console.log(import.meta.resolve("effect"))']
          : ["-e", 'console.log(import.meta.resolve("effect"))'];
      const effectPath = run(runtime, resolveArgs, v2, { env: isolation.env }).stdout.trim();
      expect(effectPath).toContain(v2);
      expect(effectPath).not.toContain("$bunfs");
    }, 120_000);
  }

  test("manifest mutation makes the modern V1 host invoke the real root twice with one daemon owner", async () => {
    const { v1 } = await ensureHostInstalls();
    const isolation = await makeIsolation("v1-root-mutation");
    const oldTmp = process.env.TMPDIR;
    process.env.TMPDIR = join(tempRoot, "subc-temp");
    await mkdir(process.env.TMPDIR, { recursive: true });
    try {
      const prepared = await prepareSubcLane();
      subcRig = await startSubcRig(prepared);
    } finally {
      if (oldTmp === undefined) delete process.env.TMPDIR;
      else process.env.TMPDIR = oldTmp;
    }
    const runtimeBefore = await subcRig.waitForAftModuleRuntime();
    const marker = join(isolation.root, "root-entry.log");
    const packageRoots: string[] = [];
    for (const label of ["root-a", "root-b"]) {
      const packageRoot = await copyInstalledPlugin(v1, label);
      const manifestPath = join(packageRoot, "package.json");
      const manifest = JSON.parse(await readFile(manifestPath, "utf8"));
      delete manifest["oc-plugin"];
      delete manifest.exports["./server"];
      manifest.main = "./load-matrix-root.mjs";
      await writeFile(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);
      await writeFile(
        join(packageRoot, "load-matrix-root.mjs"),
        `
import { appendFileSync } from "node:fs";
import actual from "./dist/index.js";
export default async function initialize(input, options) {
  appendFileSync(process.env.AFT_LOAD_MATRIX_MARKER, "root-init:${label}\\n");
  console.error("[load-matrix-host:v1-root] resolvedEntry=. init=${label}");
  const hooks = await actual(input, options);
  const dispose = hooks.dispose;
  return {
    ...hooks,
    dispose: async () => {
      appendFileSync(process.env.AFT_LOAD_MATRIX_MARKER, "root-dispose:${label}\\n");
      await dispose?.();
    },
  };
}
`,
      );
      packageRoots.push(packageRoot);
    }
    isolation.env.AFT_LOAD_MATRIX_MARKER = marker;
    await writeV1Configs(isolation, packageRoots, {
      search_index: false,
      semantic_search: false,
      tool_surface: "minimal",
    });
    const userConfigDir = join(isolation.env.XDG_CONFIG_HOME ?? "", "cortexkit");
    await mkdir(userConfigDir, { recursive: true });
    await writeFile(
      join(userConfigDir, "aft.jsonc"),
      `${JSON.stringify(
        {
          subc: { connection_file: subcRig.connectionFile },
          search_index: false,
          semantic_search: false,
          tool_surface: "minimal",
          lsp: { auto_install: false },
        },
        null,
        2,
      )}\n`,
    );

    const result = await withOperatorCanary("v1-root-mutation", () =>
      runV1ConfigHost(v1, isolation),
    );
    const transcript = `${result.stdout}\n${result.stderr}`;
    console.log(
      `[v1-root-host-transcript]\n${transcript
        .split(/\r?\n/)
        .filter((line) => line.includes("load-matrix-host"))
        .join("\n")}`,
    );
    const events = await readFile(marker, "utf8");
    expect(events.match(/root-init:/g)).toHaveLength(2);
    expect(events.match(/root-dispose:/g)).toHaveLength(2);
    expect(subcRig.daemonPid).toBeDefined();
    const runtimeAfter = await subcRig.aftModuleRuntime();
    expect(runtimeAfter?.pid).toBe(runtimeBefore.pid);
  }, 240_000);

  test("function-default entry is rejected by the V2 host loader before setup", async () => {
    const { v2 } = await ensureHostInstalls();
    const packageRoot = await copyInstalledPlugin(v2, "v2-function-negative");
    const isolation = await makeIsolation("v2-function-negative");
    const marker = join(isolation.root, "negative.log");
    const manifestPath = join(packageRoot, "package.json");
    const manifest = JSON.parse(await readFile(manifestPath, "utf8"));
    manifest.exports["./server"].import = "./function-default.mjs";
    await writeFile(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);
    await writeFile(
      join(packageRoot, "function-default.mjs"),
      `
import { appendFileSync } from "node:fs";
const entry = Object.assign(function legacyDefault() {}, {
  setup: async () => appendFileSync(process.env.AFT_LOAD_MATRIX_MARKER, "setup-called\\n"),
});
export default entry;
`,
    );
    const probe = await writeV2CoreProbe(v2, "reject");
    const result = await withOperatorCanary("v2-function-negative", () =>
      run("node", [probe, packageRoot], v2, {
        env: { ...isolation.env, AFT_LOAD_MATRIX_MARKER: marker },
      }),
    );
    const transcript = `${result.stdout}\n${result.stderr}`;
    console.log(`[v2-negative-host-transcript]\n${transcript}`);
    expect(transcript).toContain("PluginModule.LoadError");
    expect(transcript).toContain("Plugin must export a default definition");
    expect(existsSync(marker)).toBe(false);
  }, 120_000);

  test("operator OpenCode database and logs are unchanged during every host invocation", async () => {
    const operatorAfter = await snapshotOperatorState(!allowLiveOperatorWrites);
    if (allowLiveOperatorWrites) {
      expect(operatorAfter.db.size).toBe(operatorBefore.db.size);
      expect(operatorAfter.logs).toEqual(operatorBefore.logs);
    } else {
      expect(operatorAfter).toEqual(operatorBefore);
    }
    expect(hostCanaries.map(({ label }) => label)).toEqual([
      "v1-server",
      "v1-tui",
      "v2-bun",
      "v2-node",
      "v1-root-mutation",
      "v2-function-negative",
    ]);
    for (const canary of hostCanaries) {
      if (allowLiveOperatorWrites) {
        const before = canary.before as { db: PathSnapshot; logs: PathSnapshot };
        const after = canary.after as { db: PathSnapshot; logs: PathSnapshot };
        expect(after.db.size).toBe(before.db.size);
        expect(after.logs).toEqual(before.logs);
      } else {
        expect(canary.after).toEqual(canary.before);
      }
    }
  }, 300_000);
});
