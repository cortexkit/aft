import { spawn } from "node:child_process";
import { constants, type Dirent } from "node:fs";
import {
  access,
  cp,
  mkdir,
  mkdtemp,
  readdir,
  readFile,
  rm,
  stat,
  writeFile,
} from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, join, relative, resolve } from "node:path";
import {
  type AftProjectTransport,
  type AftTransportPool,
  BinaryBridge,
  type BridgeOptions,
  createAftTransportPool,
  inlineUserConfigTier,
  setActiveLogger,
} from "@cortexkit/aft-bridge";
import { hermeticGitChildEnv } from "../../../../../tests/helpers/git-env.js";
import { warmMacosExec } from "../../../../../tests/helpers/macos-exec-warm.js";
import {
  type PreparedSubcLane,
  prepareSubcLane,
  type SubcRig,
  startSubcRig,
} from "../../../../aft-bridge/src/__tests__/e2e/subc-rig.js";
import { bridgeLogger } from "../../logger.js";

// Route aft-bridge log calls (including forwarded Rust child stderr lines like
// "[aft] invalidated 7 files") into $TMPDIR/aft-plugin-test.log instead of
// console.error. Without this, every "invalidated N files" / "watcher started"
// line emitted by the Rust child during e2e tests leaks onto test stdout and
// pollutes the bash background-completion output preview.
setActiveLogger(bridgeLogger);

// Remove a temp dir, tolerating the Windows `EBUSY: resource busy or locked`
// race: a detached background-bash child (or the bridge's own handles) can keep
// the temp directory open for a brief window after shutdown, so a single `rm`
// in a test's teardown throws and fails an otherwise-passing test. Cleanup
// failures must never fail a test — retry a few times, then give up silently
// (the OS reaps the temp dir, and a leaked temp dir is harmless in CI).
/**
 * Remove scratch directories a killed run left behind. Anything older than an
 * hour cannot belong to a live suite (a single e2e project lives for seconds);
 * newer siblings may be another worker's run and are left alone.
 */
async function sweepStaleScratch(scratchRoot: string): Promise<void> {
  const cutoff = Date.now() - 60 * 60 * 1000;
  let entries: string[];
  try {
    entries = await readdir(scratchRoot);
  } catch {
    return;
  }
  await Promise.all(
    entries.map(async (entry) => {
      const path = join(scratchRoot, entry);
      try {
        if ((await stat(path)).mtimeMs < cutoff) await rm(path, { recursive: true, force: true });
      } catch {
        // another run may have removed it first
      }
    }),
  );
}

async function safeRemoveDir(dir: string): Promise<void> {
  for (let attempt = 0; attempt < 5; attempt++) {
    try {
      await rm(dir, { recursive: true, force: true });
      return;
    } catch {
      // Most commonly EBUSY on Windows while a detached child still holds a
      // handle. Back off briefly and retry; ignore if it never frees up.
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
  }
}

// Windows cargo produces `aft.exe`; Unix produces `aft`. Resolve the
// platform-correct name so CI's fail-loud "binary must be present" guard does
// not trip on a name mismatch (Windows previously silent-skipped into a false
// green before the guard landed).
const AFT_BINARY_NAME = process.platform === "win32" ? "aft.exe" : "aft";
const TARGET_DEBUG_BINARY = resolve(
  import.meta.dir,
  `../../../../../target/debug/${AFT_BINARY_NAME}`,
);
const FALLBACK_BINARY = resolve(homedir(), ".cargo/bin", AFT_BINARY_NAME);
const PROJECT_ROOT = resolve(import.meta.dir, "../../../../../");
const FIXTURES_DIR = resolve(import.meta.dir, "./fixtures");
const DEFAULT_TIMEOUT_MS = 15_000;
const SUBC_DEFAULT_SESSION_ID = "__default__";
const OUTLINE_EXTENSIONS = new Set([
  ".ts",
  ".tsx",
  ".js",
  ".jsx",
  ".mjs",
  ".cjs",
  ".rs",
  ".go",
  ".py",
  ".rb",
  ".c",
  ".cpp",
  ".h",
  ".hpp",
  ".cs",
  ".java",
  ".kt",
  ".scala",
  ".swift",
  ".lua",
  ".ex",
  ".exs",
  ".hs",
  ".sol",
  ".nix",
  ".md",
  ".mdx",
  ".css",
  ".html",
  ".json",
  ".yaml",
  ".yml",
  ".sh",
  ".bash",
]);

const GROUP_A_CONFIGURE_KEYS = new Set([
  "storage_dir",
  "harness",
  "project_root",
  "bash_permissions",
  "lsp_paths_extra",
  "lsp_auto_install_binaries",
  "lsp_inflight_installs",
  "max_background_bash_tasks",
  "aft_search_registered",
  "edit_slot_survives",
  "_ort_dylib_dir",
  "_bypass_size_limits",
]);

const LEGACY_CONFIG_KEYS = new Set([
  "experimental_bash_rewrite",
  "experimental_bash_compress",
  "experimental_bash_background",
  "bash_long_running_reminder_enabled",
  "bash_long_running_reminder_interval_ms",
  "experimental_lsp_ty",
  "lsp_servers",
  "disabled_lsp",
]);

function objectDocValue(value: unknown): Record<string, unknown> {
  return value && typeof value === "object" && !Array.isArray(value)
    ? { ...(value as Record<string, unknown>) }
    : {};
}

/**
 * E2E migration shim: keep process-state configure params flat, but wrap legacy
 * flat aft.jsonc config overrides in an inline user config tier like the plugins do.
 */
export function configureParamsFromLegacyOverrides(
  overrides: Record<string, unknown>,
): Record<string, unknown> {
  const params: Record<string, unknown> = {};
  const doc: Record<string, unknown> = {};

  for (const [key, value] of Object.entries(overrides)) {
    if (GROUP_A_CONFIGURE_KEYS.has(key)) {
      params[key] = value;
    } else if (!LEGACY_CONFIG_KEYS.has(key)) {
      doc[key] = value;
    }
  }

  const hasBashLegacy =
    Object.hasOwn(overrides, "experimental_bash_rewrite") ||
    Object.hasOwn(overrides, "experimental_bash_compress") ||
    Object.hasOwn(overrides, "experimental_bash_background") ||
    Object.hasOwn(overrides, "bash_long_running_reminder_enabled") ||
    Object.hasOwn(overrides, "bash_long_running_reminder_interval_ms");
  if (hasBashLegacy) {
    const bash = objectDocValue(doc.bash);
    bash.rewrite = Object.hasOwn(overrides, "experimental_bash_rewrite")
      ? overrides.experimental_bash_rewrite
      : false;
    bash.compress = Object.hasOwn(overrides, "experimental_bash_compress")
      ? overrides.experimental_bash_compress
      : false;
    bash.background = Object.hasOwn(overrides, "experimental_bash_background")
      ? overrides.experimental_bash_background
      : false;
    if (Object.hasOwn(overrides, "bash_long_running_reminder_enabled")) {
      bash.long_running_reminder_enabled = overrides.bash_long_running_reminder_enabled;
    }
    if (Object.hasOwn(overrides, "bash_long_running_reminder_interval_ms")) {
      bash.long_running_reminder_interval_ms = overrides.bash_long_running_reminder_interval_ms;
    }
    doc.bash = bash;
  }

  if (Object.hasOwn(overrides, "experimental_lsp_ty")) {
    const experimental = objectDocValue(doc.experimental);
    experimental.lsp_ty = overrides.experimental_lsp_ty;
    doc.experimental = experimental;
  }

  if (Object.hasOwn(overrides, "lsp_servers") || Object.hasOwn(overrides, "disabled_lsp")) {
    const lsp = objectDocValue(doc.lsp);
    if (Object.hasOwn(overrides, "lsp_servers")) lsp.servers = overrides.lsp_servers;
    if (Object.hasOwn(overrides, "disabled_lsp")) lsp.disabled = overrides.disabled_lsp;
    doc.lsp = lsp;
  }

  if (Object.keys(doc).length > 0) {
    params.config = inlineUserConfigTier(doc);
  }

  return params;
}

const SKIP_DIRS = new Set([
  "node_modules",
  ".git",
  "dist",
  "build",
  "out",
  ".next",
  ".nuxt",
  "target",
  "__pycache__",
  ".venv",
  "venv",
  "vendor",
  ".turbo",
  "coverage",
  ".nyc_output",
  ".cache",
]);

export interface PreparedBinary {
  binaryPath: string | null;
  skipReason?: string;
  source: "target" | "fallback" | null;
  buildAttempted: boolean;
}

export type HarnessTransport = "ndjson" | "subc";

export interface CreateHarnessOptions {
  fixtureNames?: string[];
  timeoutMs?: number;
  tempPrefix?: string;
  bridgeOptions?: BridgeOptions;
  transport?: HarnessTransport;
  /** Route standalone calls through the production transport factory and pool. */
  useTransportFactoryForNdjson?: boolean;
  configOverrides?: Record<string, unknown>;
}

export type HarnessFactory = (
  preparedBinary: PreparedBinary,
  options?: CreateHarnessOptions,
) => Promise<E2EHarness>;

export interface PreparedSubcHarness {
  skipReason?: string;
  buildAttempted: boolean;
}

export interface E2EHarness {
  readonly binaryPath: string;
  readonly bridge: AftProjectTransport;
  readonly tempDir: string;
  readonly transport: HarnessTransport;
  path(...segments: string[]): string;
  relativePath(...segments: string[]): string;
  cleanup(): Promise<void>;
}

export interface ReadLikePluginOptions {
  startLine?: number;
  endLine?: number;
  offset?: number;
  limit?: number;
}

let preparedBinaryPromise: Promise<PreparedBinary> | null = null;
let sharedSubcRigPromise: Promise<SubcRig> | null = null;
let sharedSubcRig: SubcRig | null = null;
let sharedSubcRigBinaryPath: string | null = null;
let sharedSubcCleanupPromise: Promise<void> | null = null;
let sharedSubcCleanupRegistered = false;

export function prepareBinary(): Promise<PreparedBinary> {
  preparedBinaryPromise ??= prepareBinaryOnce();
  return preparedBinaryPromise;
}

export async function prepareSubcHarness(
  preparedBinary: PreparedBinary,
): Promise<PreparedSubcHarness> {
  if (!preparedBinary.binaryPath) {
    return {
      buildAttempted: preparedBinary.buildAttempted,
      skipReason: preparedBinary.skipReason ?? "aft binary unavailable",
    };
  }

  const lane = await prepareSubcLane();
  if (!lane.subcCorePath) {
    return {
      buildAttempted: lane.buildAttempted,
      skipReason: lane.skipReason ?? "subc-core binary unavailable",
    };
  }

  return { buildAttempted: lane.buildAttempted || preparedBinary.buildAttempted };
}

export async function cleanupSharedSubcRig(): Promise<void> {
  if (sharedSubcCleanupPromise) return sharedSubcCleanupPromise;
  const rigPromise = sharedSubcRigPromise;
  sharedSubcRigPromise = null;
  sharedSubcRigBinaryPath = null;
  sharedSubcCleanupPromise = (async () => {
    const rig = sharedSubcRig ?? (rigPromise ? await rigPromise.catch(() => null) : null);
    sharedSubcRig = null;
    if (rig) await rig.cleanup();
  })();
  try {
    await sharedSubcCleanupPromise;
  } finally {
    sharedSubcCleanupPromise = null;
  }
}

export async function createHarness(
  preparedBinary: PreparedBinary,
  options: CreateHarnessOptions = {},
): Promise<E2EHarness> {
  if (!preparedBinary.binaryPath) {
    throw new Error(preparedBinary.skipReason ?? "aft binary unavailable");
  }

  const transport = options.transport ?? "ndjson";
  const timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
  const configOverrides = { harness: "opencode", ...(options.configOverrides ?? {}) };
  const factoryConfigOverrides = { ...configOverrides };
  // Production revalidates edit-slot survival after building its tool list.
  // Excluding the flag here makes factory-backed tests exercise the later
  // BridgePool.setConfigureOverride update rather than constructor injection.
  delete factoryConfigOverrides.edit_slot_survives;

  // Keep e2e projects outside both the repository and OS temp directories so
  // external-directory tests cover ordinary out-of-project paths (and undo
  // snapshots are not skipped as they are under /tmp). A dedicated scratch
  // root, not the repository's parent: a killed run leaves its directory
  // behind, and 76 of them once accumulated beside the user's projects.
  const scratchRoot = join(homedir(), ".cache", "aft-plugin-e2e");
  await mkdir(scratchRoot, { recursive: true });
  await sweepStaleScratch(scratchRoot);
  const tempDir = await mkdtemp(join(scratchRoot, options.tempPrefix ?? "e2e-"));

  let standaloneBridge: BinaryBridge | undefined;
  let pool: AftTransportPool | undefined;
  let bridge: AftProjectTransport | undefined;
  const subcSessions = new Set<string>();

  try {
    await copyFixturesToTempDir(tempDir, options.fixtureNames);

    // Redirect the search index cache to a temp dir so tests don't pollute the
    // user's ~/.cache/aft/index/. Pass AFT_CACHE_DIR via the bridge's per-child
    // env instead of mutating process.env: the child spawns lazily on the first
    // send(), so a process.env mutation scoped to construction would be restored
    // before the child ever inherits it — and process.env is process-global, so
    // concurrent harnesses would race. childEnv is applied at spawn time, scoped
    // to this child only.
    const poolOptions: BridgeOptions = {
      timeoutMs,
      ...(options.bridgeOptions ?? {}),
      childEnv: hermeticGitChildEnv({
        AFT_CACHE_DIR: join(tempDir, ".aft-cache"),
        ...(options.bridgeOptions?.childEnv ?? {}),
      }),
    };

    const subcRig = transport === "subc" ? await sharedSubcRigFor(preparedBinary) : undefined;
    if (subcRig || options.useTransportFactoryForNdjson) {
      if (subcRig) await writeSubcProjectConfig(tempDir, configOverrides);
      pool = await createAftTransportPool({
        harness: "opencode",
        binaryPath: preparedBinary.binaryPath,
        poolOptions,
        configOverrides: factoryConfigOverrides,
        subcConnectionFile: subcRig?.connectionFile,
        // The test runner can itself be a supervised module. Never relay that ambient
        // principal into this isolated daemon, whose launch nonce belongs to its own AFT.
        subcConsumerIdentity: null,
      });
      for (const [key, value] of Object.entries(configOverrides)) {
        pool.setConfigureOverride(key, value);
      }
      const projectBridge = pool.getBridge(tempDir);
      bridge = subcRig
        ? trackSubcSessions(projectBridge, subcSessions, tempDir, subcRig.configHome, timeoutMs)
        : projectBridge;
    } else {
      standaloneBridge = new BinaryBridge(
        preparedBinary.binaryPath,
        tempDir,
        poolOptions,
        configOverrides,
      );
      bridge = standaloneBridge;
    }
  } catch (err) {
    await pool?.shutdown().catch(() => undefined);
    await standaloneBridge?.shutdown().catch(() => undefined);
    await safeRemoveDir(tempDir);
    throw err;
  }

  return {
    binaryPath: preparedBinary.binaryPath,
    bridge,
    tempDir,
    transport,
    path: (...segments: string[]) => resolve(tempDir, ...segments),
    relativePath: (...segments: string[]) => segments.join("/"),
    cleanup: async () => {
      try {
        if (pool) {
          await Promise.allSettled(
            Array.from(subcSessions, (session) => pool?.closeSession(tempDir, session)),
          );
          await pool.shutdown();
        } else {
          await standaloneBridge?.shutdown();
        }
      } catch {
        // ignore cleanup errors
      } finally {
        await safeRemoveDir(tempDir);
      }
    },
  };
}

function registerSharedSubcCleanup(): void {
  if (sharedSubcCleanupRegistered) return;
  sharedSubcCleanupRegistered = true;
  process.once("beforeExit", () => {
    void cleanupSharedSubcRig();
  });
}

async function sharedSubcRigFor(preparedBinary: PreparedBinary): Promise<SubcRig> {
  if (!preparedBinary.binaryPath) {
    throw new Error(preparedBinary.skipReason ?? "aft binary unavailable");
  }

  if (sharedSubcRig && sharedSubcRigBinaryPath === preparedBinary.binaryPath) {
    return sharedSubcRig;
  }
  if (sharedSubcRig && sharedSubcRigBinaryPath !== preparedBinary.binaryPath) {
    await cleanupSharedSubcRig();
  }
  if (!sharedSubcRigPromise) {
    const binaryPath = preparedBinary.binaryPath;
    sharedSubcRigBinaryPath = binaryPath;
    sharedSubcRigPromise = (async () => {
      const lane = await prepareSubcLane();
      if (!lane.subcCorePath) {
        throw new Error(lane.skipReason ?? "subc-core binary unavailable");
      }
      const preparedLane: PreparedSubcLane = {
        aftBinaryPath: binaryPath,
        subcCorePath: lane.subcCorePath,
        buildAttempted: lane.buildAttempted || preparedBinary.buildAttempted,
      };
      const rig = await startSubcRig(preparedLane);
      sharedSubcRig = rig;
      registerSharedSubcCleanup();
      return rig;
    })().catch((err) => {
      sharedSubcRigPromise = null;
      sharedSubcRigBinaryPath = null;
      throw err;
    });
  }

  return sharedSubcRigPromise;
}

function trackSubcSessions(
  bridge: AftProjectTransport,
  sessions: Set<string>,
  projectRoot: string,
  configHome: string,
  defaultTimeoutMs: number,
): AftProjectTransport {
  const remember = (session: string | undefined): void => {
    sessions.add(session && session.length > 0 ? session : SUBC_DEFAULT_SESSION_ID);
  };
  const withDefaultTimeout = <T extends { timeoutMs?: number; transportTimeoutMs?: number }>(
    options: T | undefined,
  ): T | undefined => {
    if (options?.timeoutMs !== undefined || options?.transportTimeoutMs !== undefined) {
      return options;
    }
    return { ...(options ?? ({} as T)), timeoutMs: defaultTimeoutMs };
  };
  return {
    getCwd: () => bridge.getCwd(),
    getCachedStatus: () => bridge.getCachedStatus(),
    cacheStatusSnapshot: (snapshot) => bridge.cacheStatusSnapshot(snapshot),
    send: async (command, params = {}, options) => {
      remember(typeof params.session_id === "string" ? params.session_id : undefined);
      if (command === "configure") {
        await writeSubcProjectConfig(projectRoot, params);
        await writeSubcUserConfig(configHome, params);
      }
      return bridge.send(command, params, withDefaultTimeout(options));
    },
    toolCall: async (sessionId, name, rawArgs, options) => {
      remember(sessionId);
      return bridge.toolCall(sessionId, name, rawArgs, withDefaultTimeout(options));
    },
  };
}

export function harnessPool(harness: E2EHarness): AftTransportPool {
  return {
    getBridge: () => harness.bridge,
    getActiveBridgeForRoot: () => harness.bridge,
    toolCall: (_projectRoot, runtime, name, rawArgs, options) =>
      harness.bridge.toolCall(runtime.sessionID, name, rawArgs, options),
    setConfigureOverride: () => {},
    reconfigure: async () => {},
    replaceBinary: async (path) => path,
    shutdown: async () => {},
    closeSession: async () => {},
  };
}

export async function writeSubcHarnessConfig(
  harness: E2EHarness,
  configureParams: Record<string, unknown>,
): Promise<void> {
  if (harness.transport !== "subc") return;
  await writeSubcProjectConfig(harness.tempDir, configureParams);
}

async function writeSubcUserConfig(
  configHome: string,
  configureParams: Record<string, unknown>,
): Promise<void> {
  if (typeof configureParams.cortexkit_user_config_path !== "string") return;
  const doc = await projectConfigDocFromConfigureParams(configureParams);
  if (Object.keys(doc).length === 0) return;
  const configDir = join(configHome, "cortexkit");
  const configPath = join(configDir, "aft.jsonc");
  let existing: Record<string, unknown> = {};
  try {
    const parsed = JSON.parse(await readFile(configPath, "utf8")) as unknown;
    if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
      existing = parsed as Record<string, unknown>;
    }
  } catch {
    existing = {};
  }
  await mkdir(configDir, { recursive: true });
  await writeFile(configPath, JSON.stringify({ ...existing, ...doc }, null, 2), "utf8");
}

async function writeSubcProjectConfig(
  projectRoot: string,
  configureParams: Record<string, unknown>,
): Promise<void> {
  const doc = await projectConfigDocFromConfigureParams(configureParams);
  if (Object.keys(doc).length === 0) return;
  const configDir = join(projectRoot, ".cortexkit");
  await mkdir(configDir, { recursive: true });
  await writeFile(join(configDir, "aft.jsonc"), JSON.stringify(doc, null, 2), "utf8");
}

async function projectConfigDocFromConfigureParams(
  configureParams: Record<string, unknown>,
): Promise<Record<string, unknown>> {
  const doc: Record<string, unknown> = {};
  const tiers = configureParams.config;
  if (Array.isArray(tiers)) {
    for (const tier of tiers) {
      if (!tier || typeof tier !== "object") continue;
      const rawDoc = (tier as { doc?: unknown }).doc;
      if (typeof rawDoc !== "string") continue;
      try {
        const parsed = JSON.parse(rawDoc) as unknown;
        if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
          Object.assign(doc, parsed as Record<string, unknown>);
        }
      } catch {
        // Ignore invalid inline config docs; the standalone bridge will surface
        // them through configure, and subc parity should not invent a fallback.
      }
    }
  }

  const userConfigPath = configureParams.cortexkit_user_config_path;
  if (typeof userConfigPath === "string" && userConfigPath.length > 0) {
    try {
      const parsed = JSON.parse(await readFile(userConfigPath, "utf8")) as unknown;
      if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
        Object.assign(doc, parsed as Record<string, unknown>);
      }
    } catch {
      // Invalid file handling belongs to `configure`, which already reports bad
      // user config files; this subc mirror must not invent a fallback.
    }
  }

  const excluded = new Set<string>([
    ...GROUP_A_CONFIGURE_KEYS,
    ...LEGACY_CONFIG_KEYS,
    "config",
    "project_root",
    "session_id",
    "cortexkit_user_config_path",
  ]);
  for (const [key, value] of Object.entries(configureParams)) {
    if (!excluded.has(key)) doc[key] = value;
  }
  return doc;
}

export async function cleanupHarnesses(harnesses: E2EHarness[]): Promise<void> {
  await Promise.all(
    harnesses.splice(0, harnesses.length).map(async (harness) => {
      await harness.cleanup();
    }),
  );
}

export async function copyFixturesToTempDir(
  tempDir: string,
  fixtureNames?: string[],
): Promise<void> {
  const entries = fixtureNames ?? (await readdir(FIXTURES_DIR));
  await Promise.all(
    entries.map(async (entry) => {
      const source = resolve(FIXTURES_DIR, entry);
      const destination = resolve(tempDir, entry);
      await cp(source, destination, { recursive: true, force: true });
    }),
  );
}

export async function readTextFile(filePath: string): Promise<string> {
  return readFile(filePath, "utf8");
}

export function lineNumberText(text: string): string {
  return lineNumberRangeText(text, 1);
}

export function lineNumberRangeText(text: string, startLine: number, endLine?: number): string {
  const normalized = text.replace(/\r\n/g, "\n");
  const lines = normalized.split("\n");
  if (lines.at(-1) === "") {
    lines.pop();
  }

  const actualEnd = Math.min(endLine ?? lines.length, lines.length);
  const slice = lines.slice(Math.max(startLine - 1, 0), actualEnd);
  const width = String(Math.max(actualEnd, 1)).length;
  return slice
    .map((line, index) => `${String(startLine + index).padStart(width, " ")}: ${line}\n`)
    .join("");
}

export async function sendReadLikePlugin(
  bridge: AftProjectTransport,
  filePath: string,
  options: ReadLikePluginOptions = {},
): Promise<Record<string, unknown>> {
  let startLine = options.startLine;
  let endLine = options.endLine;

  if (startLine === undefined && options.offset !== undefined) {
    startLine = options.offset;
    if (options.limit !== undefined) {
      endLine = options.offset + options.limit - 1;
    }
  }

  const params: Record<string, unknown> = { file: filePath };
  if (startLine !== undefined) params.start_line = startLine;
  if (endLine !== undefined) params.end_line = endLine;
  if (options.limit !== undefined && options.offset === undefined) {
    params.limit = options.limit;
  }

  return bridge.send("read", params);
}

export async function sendOutlineDirectoryLikePlugin(
  bridge: AftProjectTransport,
  directory: string,
): Promise<Record<string, unknown>> {
  const files = await discoverOutlineFiles(directory);
  return bridge.send("outline", { files });
}

export async function sendZoomMultiSymbolLikePlugin(
  bridge: AftProjectTransport,
  filePath: string,
  symbols: string[],
  contextLines?: number,
): Promise<Array<Record<string, unknown>>> {
  return Promise.all(
    symbols.map((symbol) => {
      const params: Record<string, unknown> = { file: filePath, symbol };
      if (contextLines !== undefined) {
        params.context_lines = contextLines;
      }
      return bridge.send("zoom", params);
    }),
  );
}

export async function discoverOutlineFiles(directory: string, maxFiles = 200): Promise<string[]> {
  const files: string[] = [];

  async function walk(current: string): Promise<void> {
    if (files.length >= maxFiles) return;

    let entries: Dirent<string>[];
    try {
      entries = await readdir(current, { withFileTypes: true, encoding: "utf8" });
    } catch {
      return;
    }

    for (const entry of entries) {
      if (files.length >= maxFiles) return;

      if (entry.isDirectory()) {
        if (!SKIP_DIRS.has(entry.name) && !entry.name.startsWith(".")) {
          await walk(resolve(current, entry.name));
        }
      } else if (entry.isFile()) {
        const ext = entry.name.slice(entry.name.lastIndexOf(".")).toLowerCase();
        if (OUTLINE_EXTENSIONS.has(ext)) {
          files.push(resolve(current, entry.name));
        }
      }
    }
  }

  await walk(directory);
  files.sort();
  return files;
}

async function resolveAftBinaryPath(candidates: string[]): Promise<string | undefined> {
  for (const candidate of candidates) {
    if (await isExecutable(candidate)) {
      return candidate;
    }
  }
  return undefined;
}

function debugBinaryCandidates(): string[] {
  return [TARGET_DEBUG_BINARY];
}

function fallbackBinaryCandidates(): string[] {
  return [FALLBACK_BINARY];
}

async function prepareBinaryOnce(): Promise<PreparedBinary> {
  // Every binary this returns is about to be spawned under short per-spawn
  // timeouts, so pay macOS's one-time exec assessment here instead of inside
  // the first timed test. See warmMacosExec.
  const existing = await resolveAftBinaryPath(debugBinaryCandidates());
  if (existing) {
    await warmMacosExec(existing);
    return {
      binaryPath: existing,
      source: "target",
      buildAttempted: false,
    };
  }

  const build = await runCargoBuild();
  const built = await resolveAftBinaryPath(debugBinaryCandidates());
  if (built) {
    await warmMacosExec(built);
    return {
      binaryPath: built,
      source: "target",
      buildAttempted: true,
    };
  }

  const fallback = await resolveAftBinaryPath(fallbackBinaryCandidates());
  if (fallback) {
    await warmMacosExec(fallback);
    return {
      binaryPath: fallback,
      source: "fallback",
      buildAttempted: true,
    };
  }

  const searched = [...debugBinaryCandidates(), ...fallbackBinaryCandidates()]
    .map((path) => relative(PROJECT_ROOT, path))
    .join(" or ");
  const skipReason = build.ok
    ? `aft binary not found at ${searched}`
    : `cargo build failed and no fallback aft binary was found\n${build.output}`;

  // In CI the aft binary is always built before the Bun suites run, so a missing
  // binary there means the build/setup broke — fail loud instead of letting 25+
  // e2e files silently `describe.skipIf(!binaryPath)` into a false green. Locally
  // (CI unset) keep the quiet-skip behavior so contributors without a built
  // binary can still run the non-e2e suites.
  if (process.env.CI === "true") {
    throw new Error(
      `e2e setup failed: ${skipReason}\n` +
        "The aft binary must be present in CI (built before Bun tests run). " +
        "Refusing to silently skip e2e coverage.",
    );
  }

  return {
    binaryPath: null,
    source: null,
    buildAttempted: true,
    skipReason,
  };
}

async function isExecutable(filePath: string): Promise<boolean> {
  try {
    // Windows has no Unix execute bit; existence is enough for .exe discovery.
    const mode = process.platform === "win32" ? constants.F_OK : constants.X_OK;
    await access(filePath, mode);
    return true;
  } catch {
    return false;
  }
}

async function runCargoBuild(): Promise<{ ok: boolean; output: string }> {
  return new Promise((resolveBuild) => {
    const child = spawn("cargo", ["build"], {
      cwd: PROJECT_ROOT,
      stdio: ["ignore", "pipe", "pipe"],
    });

    let stdout = "";
    let stderr = "";

    child.stdout?.on("data", (chunk: Buffer) => {
      stdout += chunk.toString("utf8");
    });

    child.stderr?.on("data", (chunk: Buffer) => {
      stderr += chunk.toString("utf8");
    });

    child.on("error", (error) => {
      resolveBuild({ ok: false, output: error.message });
    });

    child.on("close", (code) => {
      const output = `${stdout}${stderr}`.trim();
      resolveBuild({ ok: code === 0, output });
    });
  });
}
