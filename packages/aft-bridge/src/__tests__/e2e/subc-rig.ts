import { type ChildProcessWithoutNullStreams, spawn } from "node:child_process";
import { constants } from "node:fs";
import { access, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { homedir, tmpdir } from "node:os";
import { basename, join, relative, resolve } from "node:path";

import { type CatalogEntry, SubcClient } from "@cortexkit/subc-client";

const AFT_BINARY_NAME = process.platform === "win32" ? "aft.exe" : "aft";
const SUBC_BINARY_NAME = process.platform === "win32" ? "subc-core.exe" : "subc-core";
const PROJECT_ROOT = resolve(import.meta.dir, "../../../../..");
const TARGET_DEBUG_BINARY = resolve(PROJECT_ROOT, "target", "debug", AFT_BINARY_NAME);
const FALLBACK_AFT_BINARY = resolve(homedir(), ".cargo", "bin", AFT_BINARY_NAME);
const FETCH_SUBC_CORE_SCRIPT = resolve(PROJECT_ROOT, "scripts", "fetch-subc-core.sh");
const FETCHED_SUBC_CORE_CACHE_ROOT = resolve(homedir(), ".cache", "aft-ci", "subc-core");
const DEFAULT_SUBC_RELEASE = resolve(
  homedir(),
  "Work/Projects/CortexKit/subconscious/target/release",
  SUBC_BINARY_NAME,
);
const DEFAULT_SUBC_DEBUG = resolve(
  homedir(),
  "Work/Projects/CortexKit/subconscious/target/debug",
  SUBC_BINARY_NAME,
);
const START_TIMEOUT_MS = 20_000;
const CONTROL_TIMEOUT_MS = 5_000;

export interface PreparedSubcLane {
  aftBinaryPath: string | null;
  subcCorePath: string | null;
  skipReason?: string;
  buildAttempted: boolean;
}

export interface ProjectFixtureOptions {
  /** Number of extra small files to create under nested bulk directories. */
  fileCount?: number;
  /** Number of first-level bulk directories used to spread file creation. */
  nestedDirs?: number;
}

export interface SubcSupervisorModule {
  module_id: string;
  state?: string;
  enabled?: boolean;
  live?: boolean;
  restart_count?: number;
  restartCount?: number;
  pid?: number;
  [key: string]: unknown;
}

export interface AftModuleRuntime {
  pid: number;
  command: string;
  restartCount: number | null;
  supervisor: SubcSupervisorModule | null;
  catalogReady: boolean;
}

export interface SubcRig {
  readonly tempDir: string;
  readonly homeDir: string;
  readonly configHome: string;
  readonly runtimeDir: string;
  readonly dataHome: string;
  readonly projectDir: string;
  readonly connectionFile: string;
  readonly daemonPid: number | undefined;
  createProject(name: string, options?: ProjectFixtureOptions): Promise<string>;
  listSupervisorModules(): Promise<SubcSupervisorModule[]>;
  aftModuleRuntime(): Promise<AftModuleRuntime | null>;
  waitForAftModuleRuntime(timeoutMs?: number): Promise<AftModuleRuntime>;
  waitForAftModuleRestart(
    previous: AftModuleRuntime,
    timeoutMs?: number,
  ): Promise<AftModuleRuntime>;
  waitForAftCatalog(timeoutMs?: number): Promise<void>;
  restartDaemon(): Promise<void>;
  cleanup(): Promise<void>;
}

interface DaemonDirs {
  homeDir: string;
  configHome: string;
  runtimeDir: string;
  dataHome: string;
  stderrPath: string;
  connectionFile: string;
}

interface ProcessInfo {
  pid: number;
  ppid: number;
  command: string;
}

let preparedPromise: Promise<PreparedSubcLane> | null = null;

export function prepareSubcLane(): Promise<PreparedSubcLane> {
  preparedPromise ??= prepareSubcLaneOnce();
  return preparedPromise;
}

export async function startSubcRig(prepared: PreparedSubcLane): Promise<SubcRig> {
  if (!prepared.subcCorePath || !prepared.aftBinaryPath) {
    throw new Error(prepared.skipReason ?? "subc e2e dependencies unavailable");
  }

  const tempDir = await mkdtemp(join(tmpdir(), "aft-subc-lane-"));
  const homeDir = join(tempDir, "home");
  const configHome = join(tempDir, "config");
  const runtimeDir = join(tempDir, "runtime");
  const dataHome = join(tempDir, "data");
  const projectDir = join(tempDir, "project");
  const cacheDir = join(tempDir, "cache");
  const storageDir = join(tempDir, "aft-storage");
  const connectionFile = join(runtimeDir, "subc-connection.json");
  const stderrPath = join(tempDir, "subc-core.stderr.log");

  await mkdir(join(configHome, "cortexkit"), { recursive: true });
  await Promise.all([mkdir(homeDir, { recursive: true }), mkdir(runtimeDir, { recursive: true })]);
  await Promise.all([mkdir(dataHome, { recursive: true }), mkdir(cacheDir, { recursive: true })]);

  await writeFile(
    join(configHome, "cortexkit", "aft.jsonc"),
    JSON.stringify(
      {
        storage_dir: storageDir,
        search_index: false,
        semantic_search: false,
        experimental_bash_background: true,
        bash_permissions: false,
      },
      null,
      2,
    ),
    "utf8",
  );
  await setupProjectFixture(projectDir);

  await writeFile(
    join(configHome, "cortexkit", "subc.jsonc"),
    JSON.stringify(
      {
        version: 1,
        port: 0,
        storage: { backend: "sqlite", data_home: dataHome },
        modules: {
          aft: {
            program: prepared.aftBinaryPath,
            args: [],
            env: {
              HOME: homeDir,
              XDG_CONFIG_HOME: configHome,
              XDG_DATA_HOME: dataHome,
              AFT_CACHE_DIR: cacheDir,
              AFT_CALLGRAPH_BUILD_WAIT_MS: "15000",
            },
            enabled: true,
          },
        },
      },
      null,
      2,
    ),
    "utf8",
  );

  const daemonDirs: DaemonDirs = {
    homeDir,
    configHome,
    runtimeDir,
    dataHome,
    stderrPath,
    connectionFile,
  };

  let daemon: ChildProcessWithoutNullStreams | null = null;
  try {
    daemon = await spawnReadyDaemon(prepared.subcCorePath, daemonDirs);
  } catch (err) {
    await safeRemoveDir(tempDir);
    throw err;
  }

  let cleaned = false;
  return {
    tempDir,
    homeDir,
    configHome,
    runtimeDir,
    dataHome,
    projectDir,
    connectionFile,
    get daemonPid() {
      return daemon?.pid;
    },
    createProject: async (name, options) => {
      const fixtureDir = join(tempDir, name);
      await setupProjectFixture(fixtureDir, options);
      return fixtureDir;
    },
    listSupervisorModules: () => listSupervisorModules(connectionFile),
    aftModuleRuntime: () =>
      readAftModuleRuntime(connectionFile, daemon?.pid, prepared.aftBinaryPath),
    waitForAftModuleRuntime: (timeoutMs = START_TIMEOUT_MS) =>
      waitForAftModuleRuntime(connectionFile, daemon?.pid, prepared.aftBinaryPath, timeoutMs),
    waitForAftModuleRestart: (previous, timeoutMs = 30_000) =>
      waitForAftModuleRestart(
        connectionFile,
        daemon?.pid,
        prepared.aftBinaryPath,
        previous,
        timeoutMs,
      ),
    waitForAftCatalog: (timeoutMs = START_TIMEOUT_MS) =>
      waitForAftCatalog(connectionFile, daemon, [], timeoutMs),
    restartDaemon: async () => {
      if (cleaned) throw new Error("subc rig already cleaned up");
      await stopDaemon(daemon);
      daemon = await spawnReadyDaemon(prepared.subcCorePath, daemonDirs);
    },
    cleanup: async () => {
      if (cleaned) return;
      cleaned = true;
      await stopDaemon(daemon);
      await safeRemoveDir(tempDir);
    },
  };
}

async function setupProjectFixture(
  projectDir: string,
  options: ProjectFixtureOptions = {},
): Promise<void> {
  await mkdir(join(projectDir, ".cortexkit"), { recursive: true });
  await writeFile(
    join(projectDir, ".cortexkit", "aft.jsonc"),
    JSON.stringify({ search_index: false, semantic_search: false }, null, 2),
    "utf8",
  );
  await writeFile(join(projectDir, "seed.txt"), "seed\n", "utf8");
  if (options.fileCount && options.fileCount > 0) {
    await writeBulkFixtureFiles(projectDir, options.fileCount, options.nestedDirs ?? 40);
  }
  await runGit(projectDir, ["init", "-q", "-b", "main"]);
  await runGit(projectDir, ["config", "user.email", "subc-lane@example.com"]);
  await runGit(projectDir, ["config", "user.name", "Subc Lane"]);
  await runGit(projectDir, ["config", "commit.gpgsign", "false"]);
  await runGit(projectDir, ["add", "."]);
  await runGit(projectDir, ["commit", "-q", "-m", "seed"]);
}

async function writeBulkFixtureFiles(
  projectDir: string,
  fileCount: number,
  nestedDirs: number,
): Promise<void> {
  const batch: Array<Promise<void>> = [];
  for (let i = 0; i < fileCount; i++) {
    const dir = join(
      projectDir,
      "bulk",
      `dir-${String(i % nestedDirs).padStart(2, "0")}`,
      `sub-${String(Math.floor(i / nestedDirs) % 10).padStart(2, "0")}`,
    );
    batch.push(
      (async () => {
        await mkdir(dir, { recursive: true });
        await writeFile(
          join(dir, `file-${String(i).padStart(4, "0")}.txt`),
          `bulk file ${i}\n`,
          "utf8",
        );
      })(),
    );
    if (batch.length >= 100) await Promise.all(batch.splice(0));
  }
  await Promise.all(batch);
}

async function prepareSubcLaneOnce(): Promise<PreparedSubcLane> {
  const subc = await resolveSubcCore();
  if (!subc.path) {
    return {
      aftBinaryPath: null,
      subcCorePath: null,
      skipReason: subc.skipReason,
      buildAttempted: false,
    };
  }

  const aft = await prepareAftBinary();
  if (!aft.binaryPath) {
    return {
      aftBinaryPath: null,
      subcCorePath: subc.path,
      skipReason: aft.skipReason,
      buildAttempted: aft.buildAttempted,
    };
  }

  return {
    aftBinaryPath: aft.binaryPath,
    subcCorePath: subc.path,
    buildAttempted: aft.buildAttempted,
  };
}

async function resolveSubcCore(): Promise<{ path: string | null; skipReason?: string }> {
  const envPath = process.env.SUBC_CORE_BIN?.trim();
  if (envPath) {
    if (await isExecutable(envPath)) {
      return requireWireV2Core(envPath, "SUBC_CORE_BIN", process.env.SUBC_CORE_WIRE_VERSION);
    }
    return { path: null, skipReason: `SUBC_CORE_BIN is not executable: ${envPath}` };
  }
  for (const candidate of [DEFAULT_SUBC_RELEASE, DEFAULT_SUBC_DEBUG]) {
    if (await isExecutable(candidate)) {
      return requireWireV2Core(
        candidate,
        "local subconscious build",
        process.env.SUBC_CORE_WIRE_VERSION,
      );
    }
  }

  const fetchedCache = await resolveFetchedSubcCoreCachePath();
  if (fetchedCache.path && (await isExecutable(fetchedCache.path))) {
    return requireWireV2Core(
      fetchedCache.path,
      `pinned ${await readPinnedSubcCoreTag()}`,
      await readPinnedSubcCoreWireVersion(),
    );
  }

  const siblingLocations = `${DEFAULT_SUBC_RELEASE} or ${DEFAULT_SUBC_DEBUG}`;
  if (fetchedCache.path) {
    return {
      path: null,
      skipReason:
        `subc-core binary not found at ${siblingLocations}; set SUBC_CORE_BIN, ` +
        `build the sibling subconscious checkout, or run ${relative(PROJECT_ROOT, FETCH_SUBC_CORE_SCRIPT)} ` +
        `once to populate ${fetchedCache.path}`,
    };
  }
  return {
    path: null,
    skipReason:
      `subc-core binary not found at ${siblingLocations}; set SUBC_CORE_BIN or build the sibling ` +
      `subconscious checkout (${fetchedCache.unavailableReason ?? "no cached release is available"})`,
  };
}

function requireWireV2Core(
  path: string,
  source: string,
  wireVersion: string | undefined | null,
): { path: string | null; skipReason?: string } {
  if (wireVersion?.trim() === "2") return { path };
  const reportedVersion = wireVersion?.trim() || "unknown";
  return {
    path: null,
    skipReason:
      `wire-v2 e2e requires a subc-core wire version 2 daemon; ${source} reports ` +
      `wire version ${reportedVersion}. No wire-v2 release asset is pinned yet.`,
  };
}

async function prepareAftBinary(): Promise<{
  binaryPath: string | null;
  skipReason?: string;
  buildAttempted: boolean;
}> {
  const envPath = process.env.AFT_BINARY_PATH?.trim();
  if (envPath) {
    if (await isExecutable(envPath)) {
      return { binaryPath: envPath, buildAttempted: false };
    }
    const skipReason = `AFT_BINARY_PATH is not executable: ${envPath}`;
    if (process.env.CI === "true") {
      throw new Error(`e2e setup failed: ${skipReason}`);
    }
    return { binaryPath: null, skipReason, buildAttempted: false };
  }

  if (await isExecutable(TARGET_DEBUG_BINARY)) {
    return { binaryPath: TARGET_DEBUG_BINARY, buildAttempted: false };
  }

  const build = await runCargoBuild();
  if (await isExecutable(TARGET_DEBUG_BINARY)) {
    return { binaryPath: TARGET_DEBUG_BINARY, buildAttempted: true };
  }
  if (await isExecutable(FALLBACK_AFT_BINARY)) {
    return { binaryPath: FALLBACK_AFT_BINARY, buildAttempted: true };
  }

  const searched = [TARGET_DEBUG_BINARY, FALLBACK_AFT_BINARY]
    .map((path) => relative(PROJECT_ROOT, path))
    .join(" or ");
  const skipReason = build.ok
    ? `aft binary not found at ${searched}`
    : `cargo build failed and no fallback aft binary was found\n${build.output}`;
  if (process.env.CI === "true") {
    throw new Error(`e2e setup failed: ${skipReason}`);
  }
  return { binaryPath: null, skipReason, buildAttempted: true };
}

async function resolveFetchedSubcCoreCachePath(): Promise<{
  path: string | null;
  unavailableReason?: string;
}> {
  const target = fetchedSubcCoreTarget();
  if (!target) {
    return {
      path: null,
      unavailableReason: `the pinned fetch cache only publishes darwin-arm64 and linux-x64 assets (current: ${process.platform}/${process.arch})`,
    };
  }

  const tag = await readPinnedSubcCoreTag();
  if (!tag) {
    return {
      path: null,
      unavailableReason: `could not read SUBC_CORE_TAG from ${relative(PROJECT_ROOT, FETCH_SUBC_CORE_SCRIPT)}`,
    };
  }

  return { path: resolve(FETCHED_SUBC_CORE_CACHE_ROOT, tag, `ck-subc`) };
}

function fetchedSubcCoreTarget(): string | null {
  if (process.platform === "darwin" && process.arch === "arm64") return "darwin-arm64";
  if (process.platform === "linux" && process.arch === "x64") return "linux-x64";
  return null;
}

let pinnedSubcCoreTagPromise: Promise<string | null> | null = null;
let pinnedSubcCoreWireVersionPromise: Promise<string | null> | null = null;

async function readPinnedSubcCoreTag(): Promise<string | null> {
  pinnedSubcCoreTagPromise ??= readPinnedSubcCoreTagOnce();
  return pinnedSubcCoreTagPromise;
}

async function readPinnedSubcCoreTagOnce(): Promise<string | null> {
  try {
    const script = await readFile(FETCH_SUBC_CORE_SCRIPT, "utf8");
    const match = script.match(/^SUBC_CORE_TAG=["']?([^"'\n]+)["']?$/m);
    return match?.[1] ?? null;
  } catch {
    return null;
  }
}

async function readPinnedSubcCoreWireVersion(): Promise<string | null> {
  pinnedSubcCoreWireVersionPromise ??= readPinnedSubcCoreWireVersionOnce();
  return pinnedSubcCoreWireVersionPromise;
}

async function readPinnedSubcCoreWireVersionOnce(): Promise<string | null> {
  try {
    const script = await readFile(FETCH_SUBC_CORE_SCRIPT, "utf8");
    const match = script.match(/^SUBC_CORE_WIRE_VERSION=["']?([^"'\n]+)["']?$/m);
    return match?.[1] ?? null;
  } catch {
    return null;
  }
}

async function spawnReadyDaemon(
  subcCorePath: string,
  dirs: DaemonDirs,
): Promise<ChildProcessWithoutNullStreams> {
  let lastError: unknown;
  for (let attempt = 0; attempt < 2; attempt++) {
    const daemon = spawn(subcCorePath, [], {
      env: {
        ...process.env,
        HOME: dirs.homeDir,
        XDG_CONFIG_HOME: dirs.configHome,
        XDG_RUNTIME_DIR: dirs.runtimeDir,
        XDG_DATA_HOME: dirs.dataHome,
        SUBC_PORT: "",
      },
      stdio: ["ignore", "pipe", "pipe"],
    });
    const stderrChunks: string[] = [];
    daemon.stderr.on("data", (chunk: Buffer) => {
      const text = chunk.toString("utf8");
      stderrChunks.push(text);
      void writeFile(dirs.stderrPath, stderrChunks.join(""), "utf8").catch(() => undefined);
    });
    daemon.stdout.resume();
    trackDaemon(daemon);

    try {
      await waitForAftCatalog(dirs.connectionFile, daemon, stderrChunks, START_TIMEOUT_MS);
      return daemon;
    } catch (err) {
      lastError = err;
      await stopDaemon(daemon);
      await rm(dirs.connectionFile, { force: true }).catch(() => undefined);
    }
  }
  throw lastError instanceof Error ? lastError : new Error(String(lastError));
}

async function waitForAftCatalog(
  connectionFile: string,
  daemon: ChildProcessWithoutNullStreams | null,
  stderrChunks: string[],
  timeoutMs: number,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  let lastError = "connection file not ready";
  while (Date.now() < deadline) {
    if (daemon && daemon.exitCode !== null) {
      throw new Error(
        `subc-core exited before aft registered (exit ${daemon.exitCode}); stderr:\n${stderrChunks.join("")}`,
      );
    }
    try {
      if (await catalogHasAft(connectionFile)) return;
      lastError = "catalog missing aft tool_provider";
    } catch (err) {
      lastError = err instanceof Error ? err.message : String(err);
    }
    await sleep(200);
  }
  throw new Error(`timed out waiting for aft in subc catalog: ${lastError}`);
}

async function catalogHasAft(connectionFile: string): Promise<boolean> {
  let client: SubcClient | null = null;
  try {
    client = await SubcClient.connect({ connectionFile, handshakeTimeoutMs: 1_000 });
    const entries = await client.catalogList("aft");
    return entries.some(isAftToolProvider);
  } finally {
    client?.close();
  }
}

async function listSupervisorModules(connectionFile: string): Promise<SubcSupervisorModule[]> {
  let client: SubcClient | null = null;
  try {
    client = await SubcClient.connect({ connectionFile, handshakeTimeoutMs: 1_000 });
    const reply = await client.request(
      0,
      { op: "supervisor.list" },
      { timeoutMs: CONTROL_TIMEOUT_MS },
    );
    if (!reply || typeof reply !== "object") return [];
    const modules = (reply as { modules?: unknown }).modules;
    if (!Array.isArray(modules)) return [];
    return modules.filter(isSupervisorModule);
  } finally {
    client?.close();
  }
}

async function readAftModuleRuntime(
  connectionFile: string,
  daemonPid: number | undefined,
  aftBinaryPath: string,
): Promise<AftModuleRuntime | null> {
  let modules: SubcSupervisorModule[] = [];
  let catalogReady = false;
  try {
    modules = await listSupervisorModules(connectionFile);
  } catch {
    // During daemon restarts the control socket can vanish between polls.
  }
  try {
    catalogReady = await catalogHasAft(connectionFile);
  } catch {
    catalogReady = false;
  }

  const supervisor = modules.find((module) => module.module_id === "aft") ?? null;
  const supervisorPid = numberField(supervisor?.pid);
  const processInfo = daemonPid
    ? await findAftModuleProcess(daemonPid, aftBinaryPath).catch(() => null)
    : null;
  const pid = supervisorPid ?? processInfo?.pid;
  if (pid === undefined) return null;

  return {
    pid,
    command: processInfo?.command ?? "",
    restartCount:
      numberField(supervisor?.restart_count) ?? numberField(supervisor?.restartCount) ?? null,
    supervisor,
    catalogReady,
  };
}

async function waitForAftModuleRuntime(
  connectionFile: string,
  daemonPid: number | undefined,
  aftBinaryPath: string,
  timeoutMs: number,
): Promise<AftModuleRuntime> {
  const deadline = Date.now() + timeoutMs;
  let lastRuntime: AftModuleRuntime | null = null;
  while (Date.now() < deadline) {
    lastRuntime = await readAftModuleRuntime(connectionFile, daemonPid, aftBinaryPath);
    if (lastRuntime?.catalogReady) return lastRuntime;
    await sleep(200);
  }
  throw new Error(`timed out waiting for aft module runtime; last=${JSON.stringify(lastRuntime)}`);
}

async function waitForAftModuleRestart(
  connectionFile: string,
  daemonPid: number | undefined,
  aftBinaryPath: string,
  previous: AftModuleRuntime,
  timeoutMs: number,
): Promise<AftModuleRuntime> {
  const deadline = Date.now() + timeoutMs;
  let lastRuntime: AftModuleRuntime | null = null;
  while (Date.now() < deadline) {
    lastRuntime = await readAftModuleRuntime(connectionFile, daemonPid, aftBinaryPath);
    const restartCountAdvanced =
      previous.restartCount === null ||
      lastRuntime?.restartCount === null ||
      (lastRuntime?.restartCount ?? previous.restartCount) > previous.restartCount;
    if (lastRuntime?.catalogReady && lastRuntime.pid !== previous.pid && restartCountAdvanced) {
      return lastRuntime;
    }
    await sleep(200);
  }
  throw new Error(`timed out waiting for aft module restart; last=${JSON.stringify(lastRuntime)}`);
}

async function findAftModuleProcess(
  daemonPid: number,
  aftBinaryPath: string,
): Promise<ProcessInfo | null> {
  const rows = await listProcessTable();
  const descendants = new Set<number>();
  let changed = true;
  while (changed) {
    changed = false;
    for (const row of rows) {
      if ((row.ppid === daemonPid || descendants.has(row.ppid)) && !descendants.has(row.pid)) {
        descendants.add(row.pid);
        changed = true;
      }
    }
  }
  return (
    rows.find(
      (row) => descendants.has(row.pid) && isAftProcessCommand(row.command, aftBinaryPath),
    ) ?? null
  );
}

async function listProcessTable(): Promise<ProcessInfo[]> {
  if (process.platform === "win32") return [];
  const result = await runProcess("ps", ["-eo", "pid=,ppid=,command="], PROJECT_ROOT);
  if (result.code !== 0) throw new Error(`ps failed (${result.code}): ${result.output}`);
  return result.output
    .split("\n")
    .flatMap((line): ProcessInfo[] => {
      const match = line.trim().match(/^(\d+)\s+(\d+)\s+(.*)$/);
      if (!match) return [];
      return [{ pid: Number(match[1]), ppid: Number(match[2]), command: match[3] ?? "" }];
    })
    .filter((row) => Number.isFinite(row.pid) && Number.isFinite(row.ppid));
}

function isAftProcessCommand(command: string, aftBinaryPath: string): boolean {
  const executable = command.split(/\s+/, 1)[0] ?? "";
  return (
    executable === aftBinaryPath ||
    (basename(executable) === AFT_BINARY_NAME && command.includes(" --subc "))
  );
}

function isAftToolProvider(entry: CatalogEntry): boolean {
  return (
    entry.module_id === "aft" &&
    Array.isArray(entry.roles) &&
    entry.roles.some((role) => {
      if (!role || typeof role !== "object") return false;
      return (role as { role?: unknown }).role === "tool_provider";
    })
  );
}

function isSupervisorModule(value: unknown): value is SubcSupervisorModule {
  return (
    typeof value === "object" &&
    value !== null &&
    typeof (value as { module_id?: unknown }).module_id === "string"
  );
}

function numberField(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

/**
 * Every daemon this module has spawned and not yet reaped. The process-exit
 * handler below walks it, so a runner that dies without running teardown does
 * not leave the daemon (and its module children) running.
 */
const liveDaemons = new Set<ChildProcessWithoutNullStreams>();
let exitKillerRegistered = false;

/**
 * Kill every still-running daemon when this process exits.
 *
 * `beforeExit` is not usable here: it only fires when the event loop drains on
 * its own, and a test runner ends the process with an explicit `process.exit`
 * while the daemon's stdout/stderr pipes still hold the loop open. `exit` fires
 * for explicit exits too, so this is the hook that actually runs. An `exit`
 * handler cannot await anything, so both kills are the synchronous
 * `child.kill()` form and nothing here returns a promise.
 */
function registerDaemonExitKiller(): void {
  if (exitKillerRegistered) return;
  exitKillerRegistered = true;
  process.on("exit", killLiveDaemonsOnExit);
}

function killLiveDaemonsOnExit(): void {
  const daemons = Array.from(liveDaemons);
  liveDaemons.clear();
  // Ask politely first (one pass over all of them), then make sure. On Windows
  // Node has no real signals and terminates the process for either name, which
  // is the sane no-op-equivalent behavior for this handler.
  for (const daemon of daemons) signalQuietly(daemon, "SIGTERM");
  for (const daemon of daemons) signalQuietly(daemon, "SIGKILL");
}

function signalQuietly(
  daemon: ChildProcessWithoutNullStreams,
  signal: "SIGTERM" | "SIGKILL",
): void {
  if (daemon.exitCode !== null || daemon.signalCode !== null) return;
  try {
    daemon.kill(signal);
  } catch {
    // Already gone, or the platform refused the signal; nothing left to do.
  }
}

function trackDaemon(daemon: ChildProcessWithoutNullStreams): void {
  liveDaemons.add(daemon);
  daemon.once("close", () => liveDaemons.delete(daemon));
  registerDaemonExitKiller();
}

async function stopDaemon(daemon: ChildProcessWithoutNullStreams | null): Promise<void> {
  if (!daemon) return;
  liveDaemons.delete(daemon);
  if (daemon.exitCode !== null) return;
  const close = new Promise<void>((resolveClose) => daemon.once("close", () => resolveClose()));
  daemon.kill("SIGTERM");
  const stopped = await Promise.race([close.then(() => true), sleep(2_000).then(() => false)]);
  if (!stopped && daemon.exitCode === null) {
    daemon.kill("SIGKILL");
    await Promise.race([close, sleep(2_000)]);
  }
}

async function runGit(cwd: string, args: string[]): Promise<void> {
  const result = await runProcess("git", args, cwd);
  if (result.code !== 0) {
    throw new Error(`git ${args.join(" ")} failed (${result.code}): ${result.output}`);
  }
}

async function runCargoBuild(): Promise<{ ok: boolean; output: string }> {
  const result = await runProcess("cargo", ["build"], PROJECT_ROOT);
  return { ok: result.code === 0, output: result.output };
}

async function runProcess(
  command: string,
  args: string[],
  cwd: string,
): Promise<{ code: number; output: string }> {
  return new Promise((resolveRun) => {
    const child = spawn(command, args, { cwd, stdio: ["ignore", "pipe", "pipe"] });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk: Buffer) => {
      stdout += chunk.toString("utf8");
    });
    child.stderr.on("data", (chunk: Buffer) => {
      stderr += chunk.toString("utf8");
    });
    child.on("error", (error) => resolveRun({ code: 1, output: error.message }));
    child.on("close", (code) =>
      resolveRun({ code: code ?? 1, output: `${stdout}${stderr}`.trim() }),
    );
  });
}

async function isExecutable(filePath: string): Promise<boolean> {
  try {
    const mode = process.platform === "win32" ? constants.F_OK : constants.X_OK;
    await access(filePath, mode);
    return true;
  } catch {
    return false;
  }
}

async function safeRemoveDir(dir: string): Promise<void> {
  for (let attempt = 0; attempt < 5; attempt++) {
    try {
      await rm(dir, { recursive: true, force: true });
      return;
    } catch {
      await sleep(100);
    }
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolveSleep) => setTimeout(resolveSleep, ms));
}
