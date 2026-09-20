// Every spawned host receives HOME and XDG directories under this suite's temp root.
// Otherwise the V2 client can discover the operator's OpenCode service and modify its
// database or logs. Normal runs hash those operator files before and after the matrix and
// compare metadata around each invocation. AFT_LOAD_MATRIX_ALLOW_LIVE_OPERATOR=1 is only for
// local runs beside an active operator process; it permits database content and mtime
// changes from that process while still requiring stable database size and unchanged logs.
// The CLI's V1 version probe (setup/host-generation.ts) reads the same variable the other
// way round: it is tolerant of concurrent operator writes unless the variable is exactly "0",
// because in production an OpenCode host is usually running beside the probe.
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { type ChildProcess, spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { createReadStream, existsSync, readFileSync } from "node:fs";
import { cp, mkdir, mkdtemp, readdir, readFile, rm, stat, writeFile } from "node:fs/promises";
import { createServer } from "node:net";
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
const v2Version = "2.0.3";
const v2CoreVersion = v2Version;
const allowLiveOperatorWrites = process.env.AFT_LOAD_MATRIX_ALLOW_LIVE_OPERATOR === "1";
const suiteTempParent = join(pluginRoot, "tmp");
const operatorDb = join(homedir(), ".local", "share", "opencode", "opencode.db");
const operatorLogDir = join(homedir(), ".local", "share", "opencode", "log");

function assertNodeVersion(): void {
  const result = spawnSync("node", ["--version"], {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
    shell: process.platform === "win32",
  });
  if (result.error || result.status !== 0) {
    throw new Error(
      `OpenCode V2 load matrix requires Node >= 24, but failed to execute 'node --version': ${result.error?.message ?? result.stderr}`,
    );
  }
  const version = result.stdout.trim();
  const major = Number.parseInt(version.replace(/^v/, "").split(".")[0], 10);
  if (Number.isNaN(major) || major < 24) {
    throw new Error(
      `OpenCode V2 load matrix requires Node >= 24 for '@opencode/core' await using support (found ${version}).`,
    );
  }
}
assertNodeVersion();

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
  options: { env?: NodeJS.ProcessEnv; allowFailure?: boolean; timeoutMs?: number } = {},
): CommandResult {
  const result = spawnSync(command, args, {
    cwd,
    encoding: "utf8",
    env: options.env ?? process.env,
    stdio: ["ignore", "pipe", "pipe"],
    maxBuffer: 50 * 1024 * 1024,
    shell: process.platform === "win32",
    timeout: options.timeoutMs,
  });
  if (result.status !== 0 && !options.allowFailure) {
    throw new Error(
      `${command} ${args.join(" ")} failed (${result.status ?? result.error?.message ?? "unknown"})\n${result.stdout}\n${result.stderr}`,
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
  pluginPackage: "@opencode-ai/plugin" | "@opencode/plugin",
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
          [pluginPackage]: pluginVersion,
          ...dependencies,
        },
      },
      null,
      2,
    )}\n`,
  );
  const result = run("npm", ["install", "--no-audit", "--no-fund"], root);
  const watcherDir = join(root, "node_modules", "@parcel", "watcher");
  if (existsSync(watcherDir) && !existsSync(join(watcherDir, "wrapper"))) {
    try {
      await writeFile(
        join(watcherDir, "wrapper.js"),
        await readFile(join(watcherDir, "wrapper.js")),
      );
      const wrapperTarget = join(watcherDir, "wrapper");
      if (!existsSync(wrapperTarget)) {
        await writeFile(
          wrapperTarget,
          'export * from "./wrapper.js";\nexport { default } from "./wrapper.js";\n',
        );
      }
    } catch {}
  }
  return { root, output: `${result.stdout}\n${result.stderr}` };
}

async function ensureHostInstalls(): Promise<HostInstalls> {
  hostInstalls ??= (async () => {
    const v1 = await installHost("host-v1", "@opencode-ai/plugin", modernV1, {
      "opencode-ai": modernV1,
    });
    const v2 = await installHost("host-v2", "@opencode/plugin", v2Version, {
      "@opencode/cli": v2Version,
      "@opencode/core": v2CoreVersion,
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
    OPENCODE_DB: "opencode2.db",
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

function runV1CoreLoaderHost(
  hostRoot: string,
  isolation: HostIsolation,
  options: { allowFailure?: boolean } = {},
): CommandResult {
  // The pinned V1 binary has no --standalone flag. `debug file list` boots
  // InstanceBootstrap (V1 plugin.init) and locationServices (bundled core
  // ConfigExternalPlugin) in one process, then exits.
  return run(
    v1Binary(hostRoot),
    ["--print-logs", "--log-level", "DEBUG", "debug", "file", "list", "."],
    isolation.project,
    { env: isolation.env, timeoutMs: 90_000, allowFailure: options.allowFailure },
  );
}

async function writeV1PluginSpecs(
  isolation: HostIsolation,
  specs: string[],
  aftConfig: Record<string, unknown>,
): Promise<void> {
  const configDir = join(isolation.env.XDG_CONFIG_HOME ?? "", "opencode");
  await mkdir(configDir, { recursive: true });
  await writeFile(
    join(configDir, "opencode.json"),
    `${JSON.stringify({ plugin: specs }, null, 2)}\n`,
  );
  await writeAftConfig(isolation, aftConfig);
}

async function instrumentPluginDist(packageRoot: string): Promise<void> {
  const indexPath = join(packageRoot, "dist", "index.js");
  const indexOrig = join(packageRoot, "dist", "index.orig.js");
  const serverPath = join(packageRoot, "dist", "entry", "server.js");
  const serverOrig = join(packageRoot, "dist", "entry", "server.orig.js");
  await cp(indexPath, indexOrig);
  await cp(serverPath, serverOrig);
  await writeFile(
    indexPath,
    `
import { appendFileSync } from "node:fs";
import original from "./index.orig.js";
appendFileSync(process.env.AFT_LOAD_MATRIX_MARKER, "imported=root\\n");
export default async function instrumentedRoot(...args) {
  appendFileSync(process.env.AFT_LOAD_MATRIX_MARKER, "root-called\\n");
  return original(...args);
}
`,
  );
  await writeFile(
    serverPath,
    `
import { appendFileSync } from "node:fs";
import original from "./server.orig.js";
appendFileSync(process.env.AFT_LOAD_MATRIX_MARKER, "imported=server-entry\\n");
const entry = {
  ...original,
  server: (...args) => {
    appendFileSync(process.env.AFT_LOAD_MATRIX_MARKER, "server-called\\n");
    return original.server(...args);
  },
  effect: (context) => {
    appendFileSync(process.env.AFT_LOAD_MATRIX_MARKER, "effect-called\\n");
    return original.effect(context);
  },
};
export default entry;
`,
  );
}

async function seedCoreNpmCache(
  isolation: HostIsolation,
  spec: string,
  packageRoot: string,
): Promise<void> {
  const destination = join(
    isolation.env.XDG_CACHE_HOME ?? "",
    "opencode",
    "packages",
    spec,
    "node_modules",
    "@cortexkit",
    "aft-opencode",
  );
  await mkdir(dirname(destination), { recursive: true });
  await cp(packageRoot, destination, { recursive: true });
}

async function runPinnedV1CoreLoaderRow(input: {
  label: string;
  specs: string[];
  packageRoot: string;
  seedSpec?: string;
  allowFailure?: boolean;
}): Promise<{ events: string; transcript: string }> {
  const { v1 } = await ensureHostInstalls();
  const isolation = await makeIsolation(input.label);
  const marker = join(isolation.root, "entry.log");
  isolation.env.AFT_LOAD_MATRIX_MARKER = marker;
  if (input.seedSpec) {
    await seedCoreNpmCache(isolation, input.seedSpec, input.packageRoot);
  }
  await writeV1PluginSpecs(isolation, input.specs, { enabled: false });
  const result = await withOperatorCanary(input.label, () =>
    runV1CoreLoaderHost(v1, isolation, { allowFailure: input.allowFailure }),
  );
  const transcript = `${result.stdout}\n${result.stderr}`;
  const events = existsSync(marker) ? await readFile(marker, "utf8") : "";
  console.log(
    `[${input.label}-transcript]\n${transcript
      .split(/\r?\n/)
      .filter(
        (line) =>
          line.includes("load-matrix") ||
          line.includes("AFT V2 runtime") ||
          line.includes("V2 effect skipped") ||
          line.includes("plugin"),
      )
      .join("\n")}`,
  );
  console.log(`[${input.label}-events]\n${events.trim()}`);
  return { events, transcript };
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

function freeLoopbackPort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const probe = createServer();
    probe.once("error", reject);
    probe.listen(0, "127.0.0.1", () => {
      const address = probe.address();
      if (address === null || typeof address === "string") {
        probe.close(() => reject(new Error("could not reserve a loopback port")));
        return;
      }
      probe.close(() => resolve(address.port));
    });
  });
}

function v2Binary(hostRoot: string): string {
  return join(
    hostRoot,
    "node_modules",
    ".bin",
    process.platform === "win32" ? "opencode.cmd" : "opencode",
  );
}

async function writeV2HostConfig(
  isolation: HostIsolation,
  targets: string[],
  aftConfig: Record<string, unknown>,
): Promise<void> {
  const configDir = join(isolation.env.XDG_CONFIG_HOME ?? "", "opencode");
  await mkdir(configDir, { recursive: true });
  // A throwaway HOME and four XDG roots are not enough to isolate a V2 TUI
  // host. It talks to a managed background service whose port is a constant
  // per release channel -- the release line this matrix pins always means
  // 49374 on this machine -- so two isolated runs, or a run beside the
  // operator's own editor, land on one port and the loser either fails to
  // start or silently attaches to the winner's service and its database.
  // Reserving a free port per row in the service config, which does live under
  // the isolated config root, is what keeps the runs apart.
  await writeFile(
    join(configDir, "service.json"),
    `${JSON.stringify({ hostname: "127.0.0.1", port: await freeLoopbackPort() }, null, 2)}\n`,
  );
  await writeFile(
    join(configDir, "opencode.json"),
    `${JSON.stringify({ plugin: targets }, null, 2)}\n`,
  );
  await writeAftConfig(isolation, aftConfig);
}

// Wraps the packaged TUI entry so a run can tell three states apart that the
// host's own log does not distinguish: setup never ran, setup threw, and setup
// returned but a slot render threw afterwards. `keymap.layer` is only legal
// from inside a slot render, so "the app slot rendered" is the assertion that
// actually proves the status command registered.
const v2TuiObserverEntry = `
import { appendFileSync } from "node:fs";
import original from "./src/entry/tui.mjs";

const record = (line) => appendFileSync(process.env.AFT_LOAD_MATRIX_MARKER, line + "\\n");

function observe(context) {
  // The host context exposes reactive getters, so it is proxied rather than
  // spread: spreading would freeze values the plugin re-reads later.
  const ui = new Proxy(context.ui, {
    get(target, property) {
      if (property !== "slot") return Reflect.get(target, property, target);
      return (claim) =>
        target.slot({
          ...claim,
          render: (input) => {
            try {
              const node = claim.render(input);
              record("slot-rendered:" + claim.append);
              return node;
            } catch (error) {
              record("slot-render-failed:" + claim.append + ":" + String(error));
              throw error;
            }
          },
        });
    },
  });
  return new Proxy(context, {
    get: (target, property) =>
      property === "ui" ? ui : Reflect.get(target, property, target),
  });
}

export default {
  ...original,
  setup: async (context) => {
    record("tui-setup-start");
    const cleanup = await original.setup(observe(context));
    record("tui-setup-complete");
    return cleanup;
  },
};
`;

const v2TuiSetupOutcome =
  /message="plugin operation (?:completed|failed)"[^\n]*stage=setup[^\n]*plugin=aft-opencode/;
// Reconciliation 1 runs before the configured plugins are known and 2 is the
// pass that loads them, so a third completed pass means the host settled --
// including the case where it silently skipped the target and AFT never
// appears in the log at all.
const v2TuiSettled = /message="plugin reconciliation completed"[^\n]*\bid=3\b/;

async function runV2TuiHost(input: {
  label: string;
  packageRoot: string;
  timeoutMs?: number;
}): Promise<{ transcript: string; events: string }> {
  const { v2 } = await ensureHostInstalls();
  const isolation = await makeIsolation(input.label);
  const marker = join(isolation.root, "tui-entry.log");
  isolation.env.AFT_LOAD_MATRIX_MARKER = marker;
  await writeV2HostConfig(isolation, [input.packageRoot], { enabled: false });

  const result = await withOperatorCanary(input.label, async () => {
    let stderr = "";
    // Only stderr is captured: `--print-logs` writes the plugin log there,
    // while stdout is the terminal repaint stream and grows without bound.
    const child: ChildProcess = spawn(v2Binary(v2), ["--print-logs", "--log-level", "debug"], {
      cwd: isolation.project,
      env: isolation.env,
      stdio: ["ignore", "ignore", "pipe"],
      shell: process.platform === "win32",
    });
    child.stderr?.on("data", (chunk) => {
      stderr += String(chunk);
    });
    const deadline = Date.now() + (input.timeoutMs ?? 120_000);
    const done = (): boolean => {
      const observed = existsSync(marker) ? readFileSync(marker, "utf8") : "";
      if (observed.includes("slot-rendered:app")) return true;
      // A row that does not instrument the entry has no marker to wait on, so
      // the host's own setup verdict is the stop signal.
      if (observed === "" && v2TuiSetupOutcome.test(stderr)) return true;
      return v2TuiSettled.test(stderr);
    };
    while (!done() && child.exitCode === null && Date.now() < deadline) {
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
    return stderr;
  });

  const events = existsSync(marker) ? await readFile(marker, "utf8") : "";
  const transcript = result
    .split(/\r?\n/)
    .filter((line) => line.includes("component=plugin") || line.includes("Keymap"))
    .join("\n");
  console.log(`[${input.label}-transcript]\n${transcript}`);
  console.log(`[${input.label}-events]\n${events.trim()}`);
  return { transcript: result, events };
}

async function writeV2CoreProbe(hostRoot: string, mode: "load" | "reject"): Promise<string> {
  const probe = join(hostRoot, `core-loader-${mode}.mjs`);
  await writeFile(
    probe,
    `
import { Effect } from "effect";
import { PluginModule } from "@opencode/core/plugin/module";
import { Watcher } from "@opencode/core/filesystem/watcher";
import { Host } from "@opencode/plugin/host";
import { Npm } from "@opencode/util/npm";

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
const { load } = await Effect.runPromise(
  Effect.scoped(PluginModule.make()).pipe(Effect.provide(Watcher.testLayer)),
);
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
console.log("[load-matrix-host:v2] resolvedEntry=" + entrypoints.server + " selected=effect features=" + JSON.stringify(loaded.features));`
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

async function writeV2LifecycleProbe(hostRoot: string): Promise<string> {
  const probe = join(hostRoot, "core-loader-lifecycle.mjs");
  const bridgeModule = pathToFileURL(join(repoRoot, "packages/aft-bridge/dist/index.js")).href;
  await writeFile(
    probe,
    `
import { appendFileSync, readFileSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { DatabaseSync } from "node:sqlite";
import { Effect, Exit, Fiber } from "effect";
import { PluginModule } from "@opencode/core/plugin/module";
import { Watcher } from "@opencode/core/filesystem/watcher";
import { Host } from "@opencode/plugin/host";
import { Npm } from "@opencode/util/npm";
import {
  getBridgeLifecycleTopology,
  sampleBridgeLifecycleCensus,
} from ${JSON.stringify(bridgeModule)};

const packageRoot = process.argv[2];
const directory = process.argv[3];
const marker = process.env.AFT_LOAD_MATRIX_MARKER;
const installed = {
  directory: packageRoot,
  name: ${JSON.stringify(packageName)},
  version: ${JSON.stringify(v2Version)},
  revision: "lifecycle-matrix",
};
const npm = {
  add: () => Effect.succeed(installed),
  resolve: () => Effect.succeed(installed),
  check: () => Effect.succeed(true),
  update: () => Effect.succeed(installed),
  which: () => Effect.succeed(undefined),
};
const operation = { type: "add", target: ${JSON.stringify(packageName)}, options: {} };
const { load } = await Effect.runPromise(
  Effect.scoped(PluginModule.make()).pipe(Effect.provide(Watcher.testLayer)),
);
const loaded = await Effect.runPromise(
  load(operation, { install: false }).pipe(Effect.provideService(Npm.Service, npm)),
);
if (loaded.pending) throw new Error("host loader returned pending");
const entrypoints = Host.resolve(installed);

function permissionStream() {
  const queued = [];
  const waiters = [];
  let closed = false;
  return {
    emit: (event) => {
      const waiter = waiters.shift();
      if (waiter) waiter({ done: false, value: event });
      else queued.push(event);
    },
    stream: {
      [Symbol.asyncIterator]() {
        return {
          next: () => {
            const value = queued.shift();
            if (value) return Promise.resolve({ done: false, value });
            if (closed) return Promise.resolve({ done: true, value: undefined });
            return new Promise((resolveNext) => waiters.push(resolveNext));
          },
          return: () => {
            closed = true;
            return Promise.resolve({ done: true, value: undefined });
          },
        };
      },
    },
  };
}

function locationContext(id) {
  const tools = [];
  const rpcRegistrations = [];
  const rpcEvents = [];
  let rpcDisposals = 0;
  const permissionCreates = [];
  const subscriptions = [];
  const replyPlan = ["once", "once", "reject", "reject", "once", "once"]; 
  const client = {
    event: {
      subscribe: async () => {
        const subscription = permissionStream();
        subscriptions.push(subscription);
        return { stream: subscription.stream };
      },
    },
    permission: {
      create: async (input) => {
        permissionCreates.push(input);
        const subscription = subscriptions.shift();
        if (!subscription) throw new Error("permission.create ran before event.subscribe");
        const requestID = "permission-" + permissionCreates.length;
        const reply = replyPlan.shift();
        queueMicrotask(() => subscription.emit({
          type: "permission.replied",
          properties: { sessionID: input.sessionID, requestID, reply },
        }));
        return { data: { id: requestID, effect: "ask" } };
      },
    },
  };
  return {
    tools,
    rpcRegistrations,
    rpcEvents,
    rpcDisposals: () => rpcDisposals,
    permissionCreates,
    context: {
      location: {
        directory,
        project: { id, directory, canonical: directory },
      },
      rpc: {
        register: (definition, handlers) =>
          Effect.sync(() => {
            const registration = {
              events: {
                emit: (name, payload) =>
                  Effect.sync(() => rpcEvents.push({ name, payload })),
              },
              dispose: Effect.sync(() => {
                rpcDisposals += 1;
              }),
            };
            rpcRegistrations.push({ definition, handlers, registration });
            return registration;
          }),
      },
      permission: {
        hook: () => Effect.succeed({ dispose: Effect.void }),
        list: () => Effect.succeed([]),
        get: () => Effect.succeed(undefined),
        reply: () => Effect.void,
        rules: () => Effect.void,
      },
      tool: {
        transform: (register) => Effect.sync(() => register({
          add: (tool) => tools.push(tool),
          remove: () => {},
        })),
      },
      session: {
        prompt: (input) => Effect.sync(() => {
          appendFileSync(marker, "session-prompt:" + JSON.stringify(input) + "\\n");
          return { id: "wake-" + input.sessionID };
        }),
        synthetic: (input) => Effect.sync(() => {
          appendFileSync(marker, "session-synthetic:" + JSON.stringify(input) + "\\n");
          return { id: "status-" + input.sessionID };
        }),
      },
    },
  };
}

const first = locationContext("location-a");
const second = locationContext("location-b");
await Effect.runPromise(Effect.scoped(Effect.gen(function* () {
  yield* loaded.effect(first.context);
  yield* loaded.effect(second.context);
  for (const [index, location] of [first, second].entries()) {
    const outline = location.tools.find((tool) => tool.name === "aft_outline");
    if (!outline) throw new Error("enabled V2 effect did not register aft_outline");
    if (outline.options?.codemode !== false) {
      throw new Error("enabled V2 effect did not disable CodeMode for aft_outline");
    }
    yield* outline.execute(
      { target: directory },
      {
        sessionID: "lifecycle-" + index,
        messageID: "message-" + index,
        agent: "load-matrix",
        progress: () => Effect.succeed(undefined),
      },
    );
    const rpc = location.rpcRegistrations.find(({ definition }) => definition.id === "aft");
    if (!rpc) throw new Error("enabled V2 effect did not register AftRpc");
    if (Object.keys(rpc.definition.methods).join(",") !== "getStatus") {
      throw new Error("enabled V2 effect registered unexpected AftRpc methods");
    }
    const status = yield* rpc.handlers.getStatus({ sessionID: "lifecycle-" + index });
    if (status.success === false || !status.session) {
      throw new Error("AftRpc.getStatus did not round-trip through the warm bridge: " + JSON.stringify(status));
    }
    appendFileSync(marker, "rpc-call:" + index + ":" + status.session.id + "\\n");
    if (entrypoints.rpc !== undefined || loaded.features.rpc !== undefined) {
      throw new Error("AFT unexpectedly advertised a ./rpc entrypoint");
    }
    appendFileSync(
      marker,
      "rpc-route:context.rpc.register;features.rpc=false;export.rpc=absent\\n",
    );
    appendFileSync(
      marker,
      "tools-listed:" + index + ":" + location.tools.map((tool) => tool.name).sort().join(",") + "\\n",
    );
  }
  const edit = first.tools.find((tool) => tool.name === "edit");
  const aftDelete = first.tools.find((tool) => tool.name === "aft_delete");
  if (!edit || !aftDelete) throw new Error("enabled V2 effect did not register hoisted mutation tools");
  const editPath = resolve(directory, "permission-edit.ts");
  const deletePath = resolve(directory, "permission-delete.ts");
  const deniedDeletePath = resolve(directory, "permission-delete-denied.ts");
  writeFileSync(editPath, "old\\n");
  writeFileSync(deletePath, "delete me\\n");
  writeFileSync(deniedDeletePath, "keep me\\n");
  const permissionContext = (id) => ({
    sessionID: "permission-session",
    messageID: "permission-message",
    agent: "load-matrix",
    id,
    progress: () => Effect.succeed(undefined),
  });
  const editResult = yield* edit.execute(
    { path: editPath, edits: [{ oldString: "old", newString: "new" }] },
    permissionContext("edit-refused"),
  );
  const deleteError = yield* Effect.flip(aftDelete.execute(
    { files: [deletePath] },
    permissionContext("delete-refused"),
  ));
  const permissionFailure = "did not provide a permission request endpoint";
  const editPayload = JSON.parse(editResult.content);
  if (editPayload.code !== "permission_denied" || !editPayload.message.includes(permissionFailure)) {
    throw new Error("GA edit permission classification mismatch: " + editResult.content);
  }
  if (!String(deleteError?.message).includes(permissionFailure)) {
    throw new Error("GA delete permission classification mismatch: " + String(deleteError));
  }
  if (readFileSync(editPath, "utf8") !== "old\\n" || !readFileSync(deletePath, "utf8")) {
    throw new Error("GA permission refusal allowed a filesystem mutation");
  }
  appendFileSync(
    marker,
    "permission-api:expected_fail:upstream#37164:domain=hook,list,get,reply,rules;create=absent\\n",
  );
  const live = getBridgeLifecycleTopology();
  const liveHealth = yield* Effect.promise(() => sampleBridgeLifecycleCensus({ settleMs: 0 }));
  const listeningPorts = process._getActiveHandles().flatMap((handle) => {
    if (!handle?.listening || typeof handle.address !== "function") return [];
    const address = handle.address();
    return address && typeof address === "object" && Number.isInteger(address.port)
      ? [address.port]
      : [];
  });
  appendFileSync(marker, "topology-live:" + JSON.stringify(live) + "\\n");
  appendFileSync(marker, "health-live:" + JSON.stringify(liveHealth) + "\\n");
  appendFileSync(marker, "port-census:" + JSON.stringify(listeningPorts) + "\\n");
  if (live.daemonProcesses !== 1 || live.routes !== 2 || live.locations !== 2) {
    throw new Error("unexpected live topology: " + JSON.stringify(live));
  }
  if (liveHealth.listenPorts !== 0 || listeningPorts.length !== 0) {
    throw new Error(
      "V2 started a private listening socket: " +
      JSON.stringify({ health: liveHealth, ports: listeningPorts }),
    );
  }
})))
if (first.rpcDisposals() !== 1 || second.rpcDisposals() !== 1) {
  throw new Error("Location RPC registrations were not supervisor-scoped");
}
const reloaded = locationContext("location-reload");
await Effect.runPromise(Effect.scoped(Effect.gen(function* () {
  yield* loaded.effect(reloaded.context);
  const outline = reloaded.tools.find((tool) => tool.name === "aft_outline");
  if (!outline) throw new Error("reloaded V2 effect did not register aft_outline");
  if (outline.options?.codemode !== false) {
    throw new Error("reloaded V2 effect did not disable CodeMode for aft_outline");
  }
  yield* outline.execute(
    { target: directory },
    {
      sessionID: "lifecycle-reload",
      messageID: "message-reload",
      agent: "load-matrix",
      progress: () => Effect.succeed(undefined),
    },
  );
  const rpc = reloaded.rpcRegistrations.find(({ definition }) => definition.id === "aft");
  if (!rpc) throw new Error("reloaded Location did not re-register AftRpc");
  const status = yield* rpc.handlers.getStatus({ sessionID: "lifecycle-reload" });
  if (status.success === false || status.session?.id !== "lifecycle-reload") {
    throw new Error("reloaded AftRpc.getStatus did not round-trip: " + JSON.stringify(status));
  }
  yield* rpc.registration.events.emit(
    "indexProgress",
    { index: "search", status: "ready", sessionID: "lifecycle-reload" },
  );
  if (!reloaded.rpcEvents.some(({ name }) => name === "indexProgress")) {
    throw new Error("reloaded Location did not deliver the index-progress event");
  }
  appendFileSync(marker, "rpc-reload-event:indexProgress\\n");

  const executionContext = {
    sessionID: "lifecycle-reload",
    messageID: "message-bash-abort",
    agent: "load-matrix",
    id: "bash-abort",
    progress: () => Effect.succeed(undefined),
  };
  const bash = reloaded.tools.find((tool) => tool.name === "bash");
  if (!bash) throw new Error("enabled V2 effect did not register bash");
  const abortExit = yield* Effect.exit(
    bash.execute({ command: "sleep 30", description: "cancellation probe" }, executionContext),
  );
  appendFileSync(marker, "bash-abort-exit:" + JSON.stringify(abortExit) + "\\n");
  if (!Exit.isFailure(abortExit)) {
    throw new Error("GA bash permission refusal unexpectedly started a process: " + JSON.stringify(abortExit));
  }

  const database = new DatabaseSync(
    resolve(process.env.XDG_DATA_HOME, "cortexkit", "aft", "aft.db"),
    { readOnly: true },
  );
  const abortQuery = database.prepare(
    "SELECT task_id, status, metadata FROM bash_tasks " +
      "WHERE harness = ? AND session_id = ? ORDER BY started_at DESC LIMIT 1",
  );
  const persisted = abortQuery.get("opencode", executionContext.sessionID);
  database.close();
  if (persisted !== undefined) {
    throw new Error("GA permission refusal unexpectedly persisted a bash task: " + JSON.stringify(persisted));
  }
  appendFileSync(
    marker,
    "abort-path:expected_fail:upstream#37164:permission_refused_before_process\\n",
  );
  appendFileSync(
    marker,
    "idle-wake:expected_fail:upstream#37164:permission_refused_before_background_start\\n",
  );

  const topology = getBridgeLifecycleTopology();
  appendFileSync(marker, "topology-reload:" + JSON.stringify(topology) + "\\n");
  if (topology.daemonProcesses !== 1 || topology.routes !== 1 || topology.locations !== 1) {
    throw new Error("unexpected reload topology: " + JSON.stringify(topology));
  }
})))
if (reloaded.rpcDisposals() !== 1) {
  throw new Error("reloaded RPC registration was not disposed");
}
const settled = await sampleBridgeLifecycleCensus();
appendFileSync(marker, "health-settled:" + JSON.stringify(settled) + "\\n");
if (Object.values(settled).some((count) => count !== 0)) {
  throw new Error("Location lifecycle leak after 2s settle: " + JSON.stringify(settled));
}
console.log("[load-matrix-host:v2-lifecycle] resolvedEntry=" + entrypoints.server + " features=" + JSON.stringify(loaded.features));
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
    expect(manifest["oc-plugin"]).toBeUndefined();
    expect(Object.keys(manifest.exports).sort()).toEqual([".", "./server", "./tui"]);
    expect(manifest.dependencies.effect).toBe("4.0.0-rc.112");
    expect(manifest.peerDependencies["@opencode-ai/plugin"]).toBe(">=0.0.0-beta-0");
    expect(manifest.peerDependencies["@opencode/plugin"]).toBe("2.0.3");
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
  test("packed installs have no peer warning under modern V1 and the GA host", async () => {
    const installs = await ensureHostInstalls();
    expect(installs.v1Output).not.toContain("ERESOLVE");
    expect(installs.v2Output).not.toContain("ERESOLVE");
    expect(existsSync(v1Binary(installs.v1))).toBe(true);
    expect(
      existsSync(
        join(installs.v2, "node_modules", "@opencode", "core", "dist", "plugin", "module.js"),
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
    const serverResolutions = events.match(/resolvedEntry=\.\/server/g) ?? [];
    expect(serverResolutions.length).toBeGreaterThan(0);
    expect(events).not.toContain("effect-called");
    expect(events).not.toContain("setup-called");
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
      expect(transcript).toContain('selected=effect features={"tui":true}');
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

  test("enabled V2 loader shares one daemon across two Locations and reloads without leaks", async () => {
    const { v2 } = await ensureHostInstalls();
    const packageRoot = await copyInstalledPlugin(v2, "v2-lifecycle");
    const isolation = await makeIsolation("v2-lifecycle");
    const marker = join(isolation.root, "lifecycle.log");
    const binaryPath =
      process.env.AFT_BINARY_PATH?.trim() ||
      join(repoRoot, "target", "debug", process.platform === "win32" ? "aft.exe" : "aft");
    expect(existsSync(binaryPath)).toBe(true);
    await writeAftConfig(isolation, {
      enabled: true,
      search_index: false,
      semantic_search: false,
      tool_surface: "all",
      hoist_builtin_tools: true,
      bash: true,
      lsp: { auto_install: false },
    });
    const manifestPath = join(packageRoot, "package.json");
    const manifest = JSON.parse(await readFile(manifestPath, "utf8"));
    manifest.exports["./server"].import = "./load-matrix-lifecycle-server.mjs";
    await writeFile(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);
    await writeFile(
      join(packageRoot, "load-matrix-lifecycle-server.mjs"),
      `
import { appendFileSync } from "node:fs";
import { Effect } from "effect";
import original from "./dist/entry/server.js";
const effect = (context) => Effect.gen(function* () {
  yield* Effect.sync(() => appendFileSync(process.env.AFT_LOAD_MATRIX_MARKER, "effect-init\\n"));
  yield* Effect.addFinalizer(() =>
    Effect.sync(() => appendFileSync(process.env.AFT_LOAD_MATRIX_MARKER, "effect-dispose\\n")),
  );
  yield* original.effect(context);
});
export default { id: original.id, effect };
`,
    );
    const probe = await writeV2LifecycleProbe(v2);
    const result = await withOperatorCanary("v2-lifecycle", () =>
      run("node", [probe, packageRoot, isolation.project], v2, {
        env: {
          ...isolation.env,
          AFT_BINARY_PATH: binaryPath,
          AFT_LOAD_MATRIX_MARKER: marker,
        },
      }),
    );
    const transcript = `${result.stdout}\n${result.stderr}`;
    console.log(`[v2-lifecycle-host-transcript]\n${transcript}`);
    expect(transcript).toContain("[load-matrix-host:v2-lifecycle]");
    const events = await readFile(marker, "utf8");
    const evidence = events.split(/\r?\n/);
    const abortEvidence = evidence.filter((line) => line.startsWith("abort-path:"));
    const wakeEvidence = evidence.filter((line) => line.startsWith("idle-wake:"));
    const permissionEvidence = evidence.filter((line) => line.startsWith("permission-api:"));
    const rpcEvidence = evidence.filter(
      (line) =>
        line.startsWith("rpc-call:") ||
        line.startsWith("rpc-reload-event:") ||
        line.startsWith("rpc-route:"),
    );
    const toolEvidence = evidence.filter((line) => line.startsWith("tools-listed:"));
    console.log(`[v2-abort-evidence]\n${abortEvidence.join("\n")}`);
    console.log(`[v2-wake-evidence]\n${wakeEvidence.join("\n")}`);
    console.log(`[v2-permission-evidence]\n${permissionEvidence.join("\n")}`);
    console.log(`[v2-rpc-evidence]\n${rpcEvidence.join("\n")}`);
    console.log(`[v2-tool-evidence]\n${toolEvidence.join("\n")}`);
    expect(abortEvidence).toEqual([
      "abort-path:expected_fail:upstream#37164:permission_refused_before_process",
    ]);
    expect(wakeEvidence).toEqual([
      "idle-wake:expected_fail:upstream#37164:permission_refused_before_background_start",
    ]);
    expect(permissionEvidence).toEqual([
      "permission-api:expected_fail:upstream#37164:domain=hook,list,get,reply,rules;create=absent",
    ]);
    expect(rpcEvidence.filter((line) => line.startsWith("rpc-call:"))).toHaveLength(2);
    expect(rpcEvidence).toContain(
      "rpc-route:context.rpc.register;features.rpc=false;export.rpc=absent",
    );
    expect(rpcEvidence).toContain("rpc-reload-event:indexProgress");
    expect(toolEvidence).toHaveLength(2);
    expect(events.match(/effect-init/g)).toHaveLength(3);
    expect(events.match(/effect-dispose/g)).toHaveLength(3);
    expect(transcript).toContain('features={"tui":true}');
    expect(events).toContain("tools-listed:0:");
    expect(events).toContain("aft_outline");
    expect(events).toContain(
      'topology-reload:{"daemonProcesses":1,"routes":1,"subcClients":0,"locations":1}',
    );
    const liveHealthLine = events.split(/\r?\n/).find((line) => line.startsWith("health-live:"));
    expect(liveHealthLine).toBeDefined();
    if (!liveHealthLine) throw new Error("load matrix did not record live health");
    const liveHealth = JSON.parse(liveHealthLine.slice("health-live:".length));
    expect(liveHealth.listenPorts).toBe(0);
    expect(events).toContain("port-census:[]");
    expect(events).toContain("rpc-call:0:lifecycle-0");
    expect(events).toContain("rpc-call:1:lifecycle-1");
    expect(events).toContain("rpc-reload-event:indexProgress");
    expect(events).toContain('bash-abort-exit:{"_id":"Exit","_tag":"Failure"');
    expect(events).not.toContain("bash-abort-row:");
    expect(events).not.toContain("background-start:");
    expect(events).not.toContain("session-prompt:");
    expect(events).toContain(
      'health-settled:{"watchers":0,"listenPorts":0,"routes":0,"lspChildren":0,"daemonProcesses":0}',
    );
    expect(events).not.toContain("permissions:");
  }, 180_000);

  test("GA TUI host mounts the sidebar and its status command without a keymap error", async () => {
    const { v2 } = await ensureHostInstalls();
    const packageRoot = await copyInstalledPlugin(v2, "v2-tui-keymap");
    await writeFile(join(packageRoot, "tui.js"), v2TuiObserverEntry);

    const { transcript, events } = await runV2TuiHost({
      label: "v2-tui-keymap",
      packageRoot,
    });

    // The host reports a throwing setup as a failed plugin operation and puts
    // the message on a toast; both AFT's own registration and every later
    // registration in the same setup are lost with it.
    expect(transcript).not.toContain("Keymap.Provider is missing");
    expect(transcript).not.toMatch(/message="plugin operation failed"[^\n]*plugin=aft-opencode/);
    expect(transcript).toMatch(
      /message="plugin operation completed"[^\n]*stage=setup[^\n]*plugin=aft-opencode/,
    );

    expect(events).toContain("tui-setup-start\n");
    expect(events).toContain("tui-setup-complete\n");
    // The status command is owned by the component rendered into `app`, so a
    // completed render of that slot is what proves `keymap.layer` succeeded
    // where the host allows it.
    expect(events).toContain("slot-rendered:app\n");
    expect(events).not.toContain("slot-render-failed:");
  }, 240_000);

  test("GA TUI host loads a directory target through its root tui entrypoint", async () => {
    const { v2 } = await ensureHostInstalls();
    const packageRoot = await copyInstalledPlugin(v2, "v2-tui-directory");

    // The target is the package directory itself: not the package name, which
    // would resolve through the exports map, and not an entry file, which this
    // host skips outright. A directory is resolved by joining `tui` onto it, so
    // the file that has to exist is `<dir>/tui.js` in the package root. Every
    // resolution failure there is swallowed, so without that file the TUI
    // feature disappears with nothing in the log to explain it.
    const { transcript, events } = await runV2TuiHost({
      label: "v2-tui-directory",
      packageRoot,
    });

    expect(transcript).toContain(`entrypoint=${pathToFileURL(join(packageRoot, "tui.js")).href}`);
    expect(transcript).toContain(`target=${packageRoot}`);
    // Whether setup then succeeds is the sibling row's subject. This row asks
    // only whether the directory branch reached the entry at all, so it stops
    // at the host having called into it.
    expect(transcript).toMatch(
      /message="plugin operation started"[^\n]*stage=setup[^\n]*plugin=aft-opencode/,
    );
    // This row deliberately runs the shipped entry, so nothing instruments it.
    expect(events).toBe("");
  }, 240_000);

  test("manifest mutation converges multiple V1 root invocations on one daemon owner", async () => {
    // This is the one row that needs a real subconscious daemon. The sibling
    // subc lanes skip their whole describe when the core binary is absent
    // (it lives in a private repository and is never on a CI runner); this
    // row records the same skip as a canary entry so the label roll-call
    // below still proves every row ran or named why it did not.
    const preparedProbe = await prepareSubcLane();
    if (preparedProbe.skipReason) {
      hostCanaries.push({
        label: "v1-root-mutation",
        before: `skipped: ${preparedProbe.skipReason}`,
        after: `skipped: ${preparedProbe.skipReason}`,
      });
      console.log(`[load-matrix] v1-root-mutation skipped: ${preparedProbe.skipReason}`);
      return;
    }
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
    const initCount = events.match(/root-init:/g)?.length ?? 0;
    const disposeCount = events.match(/root-dispose:/g)?.length ?? 0;
    expect(initCount).toBeGreaterThan(0);
    expect(disposeCount).toBe(initCount);
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

  test("pinned V1 core loader ignores the bare package spec's V2 effect", async () => {
    const { v1 } = await ensureHostInstalls();
    const packageRoot = await copyInstalledPlugin(v1, "v1-core-bare");
    await instrumentPluginDist(packageRoot);
    const manifest = JSON.parse(await readFile(join(packageRoot, "package.json"), "utf8"));
    const spec = `${packageName}@${manifest.version}`;
    const { events, transcript } = await runPinnedV1CoreLoaderRow({
      label: "v1-core-bare",
      specs: [spec],
      packageRoot,
      seedSpec: spec,
    });
    // V1 appends ./server and calls it. The bundled core loader resolves the
    // package name with import.meta.resolve against the install dir and may
    // skip the import entirely; either way the V2 effect must not run.
    expect(events).toContain("imported=server-entry");
    expect(events).toContain("server-called");
    expect(events).not.toContain("effect-called");
    expect(events).not.toContain("setup-called");
    expect(transcript).not.toContain("AFT V2 runtime starting");
  }, 120_000);

  test("pinned V1 core loader ignores a file:// package directory's V2 effect", async () => {
    const { v1 } = await ensureHostInstalls();
    const packageRoot = await copyInstalledPlugin(v1, "v1-core-file");
    await instrumentPluginDist(packageRoot);
    const { events, transcript } = await runPinnedV1CoreLoaderRow({
      label: "v1-core-file",
      specs: [pathToFileURL(packageRoot).href],
      packageRoot,
    });
    expect(events).toContain("imported=root");
    expect(events).toContain("server-called");
    expect(events).not.toContain("effect-called");
    expect(events).not.toContain("setup-called");
    expect(transcript).not.toContain("AFT V2 runtime starting");
  }, 120_000);

  test("pinned V1 core loader cannot run the V2 effect body for a file:// server entry", async () => {
    const { v1 } = await ensureHostInstalls();
    const packageRoot = await copyInstalledPlugin(v1, "v1-core-server-file");
    await instrumentPluginDist(packageRoot);
    const serverEntry = join(packageRoot, "dist", "entry", "server.js");
    const { events, transcript } = await runPinnedV1CoreLoaderRow({
      label: "v1-core-server-file",
      specs: [pathToFileURL(serverEntry).href],
      packageRoot,
    });
    expect(events).toContain("imported=server-entry");
    expect(events).toContain("effect-called");
    expect(events).toContain("server-called");
    expect(events).not.toContain("setup-called");
    expect(transcript).not.toContain("AFT V2 runtime starting");
    expect(transcript).toContain("V2 effect skipped: host context has no location");
  }, 120_000);

  test("pinned V1 core loader does not import a package subpath spec", async () => {
    const { v1 } = await ensureHostInstalls();
    const packageRoot = await copyInstalledPlugin(v1, "v1-core-subpath");
    await instrumentPluginDist(packageRoot);
    const { events, transcript } = await runPinnedV1CoreLoaderRow({
      label: "v1-core-subpath",
      specs: [`${packageName}/server`],
      packageRoot,
      allowFailure: true,
    });
    expect(events).not.toContain("imported=root");
    expect(events).not.toContain("imported=server-entry");
    expect(events).not.toContain("effect-called");
    expect(events).not.toContain("server-called");
    expect(transcript).not.toContain("AFT V2 runtime starting");
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
      "v2-lifecycle",
      "v2-tui-keymap",
      "v2-tui-directory",
      "v1-root-mutation",
      "v2-function-negative",
      "v1-core-bare",
      "v1-core-file",
      "v1-core-server-file",
      "v1-core-subpath",
    ]);
    for (const canary of hostCanaries) {
      if (typeof canary.before === "string") {
        // A skipped row records its reason in place of snapshots; there is
        // no operator state to compare for it.
        expect(canary.after).toEqual(canary.before);
        continue;
      }
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
