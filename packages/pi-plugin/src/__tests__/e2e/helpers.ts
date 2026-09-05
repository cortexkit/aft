/**
 * End-to-end test harness for @cortexkit/aft-pi.
 *
 * Spins up a real `aft` binary via BinaryBridge + BridgePool (identical to
 * production transport), registers every Pi tool wrapper with a mock
 * ExtensionAPI, and lets tests drive each tool's execute() directly.
 *
 * This is the layer where production bugs like wrong Rust command names
 * actually surface — the bridge itself is a direct copy of opencode-plugin
 * which has its own e2e coverage.
 */

import { spawn } from "node:child_process";
import { constants } from "node:fs";
import { access, cp, mkdir, mkdtemp, readdir, rm, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { join, relative, resolve } from "node:path";
import type { BinaryBridge } from "@cortexkit/aft-bridge";
import { BridgePool, inlineUserConfigTier, setActiveLogger } from "@cortexkit/aft-bridge";
import { hermeticGitChildEnv, withHermeticGitEnv } from "../../../../../tests/helpers/git-env.js";
import { warmMacosExec } from "../../../../../tests/helpers/macos-exec-warm.js";
import { bridgeLogger } from "../../logger.js";

// Route aft-bridge log calls (including forwarded Rust child stderr lines like
// "[aft] invalidated 7 files") into $TMPDIR/aft-pi-test.log instead of
// console.error. Without this, every "invalidated N files" / "watcher started"
// line emitted by the Rust child during e2e tests leaks onto test stdout and
// pollutes the bash background-completion output preview.
setActiveLogger(bridgeLogger);

import type { AftConfig } from "../../config.js";
import { registerAstTools } from "../../tools/ast.js";
import { registerConflictsTool } from "../../tools/conflicts.js";
import { registerFsTools } from "../../tools/fs.js";
import { registerHoistedTools } from "../../tools/hoisted.js";
import { registerImportTools } from "../../tools/imports.js";
import { registerNavigateTool } from "../../tools/navigate.js";
import { registerReadingTools } from "../../tools/reading.js";
import { registerSafetyTool } from "../../tools/safety.js";
import { registerSemanticTool } from "../../tools/semantic.js";
import type { PluginContext } from "../../types.js";

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

// Minimal AgentToolResult shape — pi-agent-core defines
//   { content: (TextContent | ImageContent)[]; details: T }
// We only need to read text content in assertions.
export interface TextContent {
  type: "text";
  text: string;
}
export interface AgentToolResultLike {
  content: Array<TextContent | { type: string; [k: string]: unknown }>;
  details?: unknown;
}

/** Mock ToolDefinition shape — matches ExtensionAPI.registerTool's parameter. */
export interface MockToolDef {
  name: string;
  label: string;
  description: string;
  parameters: unknown;
  execute: (
    toolCallId: string,
    params: unknown,
    signal: AbortSignal | undefined,
    onUpdate: ((update: unknown) => void) | undefined,
    ctx: MockExtensionContext,
  ) => Promise<AgentToolResultLike>;
}

/** Mock ExtensionContext — only fields the Pi tool wrappers actually read. */
export interface MockExtensionContext {
  cwd: string;
  hasUI: boolean;
  signal?: AbortSignal;
}

export interface PreparedBinary {
  binaryPath: string | null;
  skipReason?: string;
}

let preparedBinaryPromise: Promise<PreparedBinary> | null = null;

export function prepareBinary(): Promise<PreparedBinary> {
  preparedBinaryPromise ??= prepareBinaryOnce();
  return preparedBinaryPromise;
}

export interface Harness {
  readonly binaryPath: string;
  readonly bridge: BinaryBridge;
  readonly pool: BridgePool;
  readonly tempDir: string;
  readonly tools: ReadonlyMap<string, MockToolDef>;
  readonly extCtx: MockExtensionContext;
  /** Absolute path inside the temp dir. */
  path(...segments: string[]): string;
  /** Relative path as agents would pass it. */
  relPath(...segments: string[]): string;
  /** Call a registered tool by name. Throws if not registered. */
  callTool(name: string, params: Record<string, unknown>): Promise<AgentToolResultLike>;
  /** Extract flattened text from a result's content array. */
  text(result: AgentToolResultLike): string;
  cleanup(): Promise<void>;
}

export interface HarnessOptions {
  /** Which fixture files/directories under fixtures/ to copy. Defaults to all. */
  fixtureNames?: string[];
  /** Override tool-surface-affecting config. */
  config?: Partial<AftConfig>;
  /** Override bridge timeout. */
  timeoutMs?: number;
  /** Skip copying fixtures. */
  noFixtures?: boolean;
}

/**
 * Build a harness with all tools registered.
 *
 * Tests should:
 *   const prep = await prepareBinary();
 *   if (!prep.binaryPath) return; // skip if no binary
 *   const harness = await createHarness(prep, { ... });
 *   try { ... } finally { await harness.cleanup(); }
 */
export async function createHarness(
  preparedBinary: PreparedBinary,
  options: HarnessOptions = {},
): Promise<Harness> {
  if (!preparedBinary.binaryPath) {
    throw new Error(preparedBinary.skipReason ?? "aft binary unavailable");
  }

  const testRoot = join(homedir(), ".cache");
  await mkdir(testRoot, { recursive: true });
  const tempDir = await mkdtemp(join(testRoot, "aft-pi-e2e-"));

  try {
    if (!options.noFixtures) {
      await copyFixturesToTempDir(tempDir, options.fixtureNames);
    }
  } catch (err) {
    await rm(tempDir, { recursive: true, force: true });
    throw err;
  }

  // Full permissive surface so registerAllTools exposes every tool by default.
  const config: AftConfig = {
    tool_surface: "all",
    format_on_edit: false,
    validate_on_edit: "syntax",
    search_index: true,
    semantic_search: false,
    restrict_to_project_root: false,
    ...(options.config ?? {}),
  };

  // Redirect AFT caches/indexes to temp so tests don't pollute user data.
  // Pass AFT_CACHE_DIR via the bridge's per-child env (childEnv) rather than
  // mutating process.env: bridges spawn lazily and process.env is process-global,
  // so a construction-scoped mutation would race concurrent harnesses and be
  // restored before the child inherits it. childEnv is applied at spawn time,
  // scoped to this child only.
  const pool = new BridgePool(
    preparedBinary.binaryPath,
    {
      timeoutMs: options.timeoutMs ?? DEFAULT_TIMEOUT_MS,
      // AFT_CALLGRAPH_BUILD_WAIT_MS makes callgraph ops block until the
      // background store build finishes (default 0 = non-blocking, returns
      // callgraph_building). Tests need the store ready synchronously; fixtures
      // are tiny so a few seconds is ample headroom.
      childEnv: hermeticGitChildEnv({
        AFT_CACHE_DIR: join(tempDir, ".aft-cache"),
        AFT_CALLGRAPH_BUILD_WAIT_MS: "15000",
      }),
    },
    // Forward the full config to configure so indexing/restrict/etc. match prod.
    configureParamsFromLegacyOverrides({
      ...config,
      storage_dir: join(tempDir, ".aft-storage"),
      harness: "pi",
    }),
  );

  const bridge = pool.getBridge(tempDir);
  const storageDir = join(tempDir, ".aft-storage");
  const ctx: PluginContext = { pool, config, storageDir };

  const tools = new Map<string, MockToolDef>();
  const api = makeMockApi(tools);

  // Permissive surface — every tool wired up. Mirrors resolveToolSurface("all").
  const surface = {
    hoistBash: true,
    hoistRead: true,
    hoistWrite: true,
    hoistEdit: true,
    // Pi's built-in grep is always present; we always override with AFT's indexed version.
    hoistGrep: true,
    outline: true,
    zoom: true,
    semantic: config.semantic_search === true,
    navigate: true,
    conflicts: true,
    importTool: true,
    safety: true,
    delete: true,
    move: true,
    astSearch: true,
    astReplace: true,
    // E2E surface defaults to restricted mode so the existing tests that
    // expect ui.confirm prompts for external paths keep working. The new
    // regression test in hoisted.test.ts toggles this flag explicitly.
    restrictToProjectRoot: true,
  };

  registerHoistedTools(api, ctx, surface);
  registerReadingTools(api, ctx, surface);
  if (surface.semantic) registerSemanticTool(api, ctx);
  registerNavigateTool(api, ctx);
  registerConflictsTool(api, ctx);
  registerImportTools(api, ctx);
  registerSafetyTool(api, ctx);
  registerAstTools(api, ctx, surface);
  registerFsTools(api, ctx, surface);

  const extCtx: MockExtensionContext = {
    cwd: tempDir,
    hasUI: false,
  };

  return {
    binaryPath: preparedBinary.binaryPath,
    bridge,
    pool,
    tempDir,
    tools,
    extCtx,
    path: (...segments: string[]) => resolve(tempDir, ...segments),
    relPath: (...segments: string[]) => segments.join("/"),
    callTool: async (name: string, params: Record<string, unknown>) => {
      const tool = tools.get(name);
      if (!tool) {
        throw new Error(
          `Tool '${name}' not registered. Available: ${Array.from(tools.keys()).sort().join(", ")}`,
        );
      }
      return tool.execute(`test-${name}-${Date.now()}`, params, undefined, undefined, extCtx);
    },
    text: (result: AgentToolResultLike) => flattenText(result),
    cleanup: async () => {
      try {
        await pool.shutdown();
      } catch {
        // ignore
      } finally {
        await rm(tempDir, { recursive: true, force: true }).catch(() => {});
      }
    },
  };
}

/**
 * Build a barebones fixture Pi would never see but helpers can use for ad-hoc tests.
 */
export async function writeFixture(
  harness: Harness,
  relativePath: string,
  content: string,
): Promise<string> {
  const absPath = harness.path(relativePath);
  const parent = resolve(absPath, "..");
  await mkdir(parent, { recursive: true }).catch(() => {});
  await writeFile(absPath, content, "utf8");
  return absPath;
}

/**
 * Create a real git repo inside the harness temp dir with a merge conflict
 * in the given file. Used by aft_conflicts tests.
 */
export async function createConflictRepo(harness: Harness, relativePath: string): Promise<string> {
  const dir = harness.tempDir;
  await runGit(dir, ["init", "-q", "-b", "main"]);
  await runGit(dir, ["config", "user.email", "test@example.com"]);
  await runGit(dir, ["config", "user.name", "Test"]);
  await runGit(dir, ["config", "commit.gpgsign", "false"]);

  const absPath = await writeFixture(harness, relativePath, "line1\nshared\nline3\n");
  await runGit(dir, ["add", "."]);
  await runGit(dir, ["commit", "-q", "-m", "init"]);

  await runGit(dir, ["checkout", "-q", "-b", "branch-a"]);
  await writeFixture(harness, relativePath, "line1\nfrom-a\nline3\n");
  await runGit(dir, ["commit", "-q", "-am", "change-a"]);

  await runGit(dir, ["checkout", "-q", "main"]);
  await runGit(dir, ["checkout", "-q", "-b", "branch-b"]);
  await writeFixture(harness, relativePath, "line1\nfrom-b\nline3\n");
  await runGit(dir, ["commit", "-q", "-am", "change-b"]);

  // Merge branch-a into branch-b → produces a conflict in `relativePath`.
  const mergeResult = await runGitCapture(dir, ["merge", "--no-edit", "branch-a"]);
  if (mergeResult.code === 0) {
    throw new Error(`Expected conflict merging branch-a into branch-b, but merge succeeded`);
  }
  return absPath;
}

async function runGit(cwd: string, args: string[]): Promise<void> {
  const res = await runGitCapture(cwd, args);
  if (res.code !== 0) {
    throw new Error(`git ${args.join(" ")} failed (${res.code}): ${res.output}`);
  }
}

async function runGitCapture(
  cwd: string,
  args: string[],
): Promise<{ code: number; output: string }> {
  return new Promise((resolveCmd) => {
    const child = spawn("git", args, {
      cwd,
      env: withHermeticGitEnv(),
      stdio: ["ignore", "pipe", "pipe"],
    });
    let stdout = "";
    let stderr = "";
    child.stdout?.on("data", (c: Buffer) => {
      stdout += c.toString("utf8");
    });
    child.stderr?.on("data", (c: Buffer) => {
      stderr += c.toString("utf8");
    });
    child.on("close", (code) => {
      resolveCmd({ code: code ?? 1, output: `${stdout}${stderr}`.trim() });
    });
    child.on("error", (err) => {
      resolveCmd({ code: 1, output: err.message });
    });
  });
}

function flattenText(result: AgentToolResultLike): string {
  if (!result || !Array.isArray(result.content)) return "";
  return result.content
    .filter((c): c is TextContent => c.type === "text" && typeof c.text === "string")
    .map((c) => c.text)
    .join("\n");
}

// ExtensionAPI has 20+ methods tests don't exercise; any is intentional here
// (test overrides in biome.json permit noExplicitAny).
type AnyExtensionApi = any;

function makeMockApi(tools: Map<string, MockToolDef>): AnyExtensionApi {
  // Proxy returns a no-op for any unknown method, so the mock covers ExtensionAPI's
  // full surface without hardcoding every method signature.
  return new Proxy(
    {
      registerTool(tool: MockToolDef): void {
        tools.set(tool.name, tool);
      },
    },
    {
      get(target: Record<string, unknown>, prop: string): unknown {
        if (prop in target) return target[prop];
        // Unknown methods (registerCommand, on, off, registerShortcut, etc.) → no-op.
        return () => undefined;
      },
    },
  );
}

export async function copyFixturesToTempDir(
  tempDir: string,
  fixtureNames?: string[],
): Promise<void> {
  let entries: string[];
  try {
    entries = fixtureNames ?? (await readdir(FIXTURES_DIR));
  } catch {
    return; // fixtures dir missing is OK — tests can write inline fixtures.
  }
  await Promise.all(
    entries.map(async (entry) => {
      const source = resolve(FIXTURES_DIR, entry);
      const destination = resolve(tempDir, entry);
      await cp(source, destination, { recursive: true, force: true });
    }),
  );
}

async function prepareBinaryOnce(): Promise<PreparedBinary> {
  // Every binary this returns is about to be spawned under short per-spawn
  // timeouts, so pay macOS's one-time exec assessment here instead of inside
  // the first timed test. See warmMacosExec.
  if (await isExecutable(TARGET_DEBUG_BINARY)) {
    await warmMacosExec(TARGET_DEBUG_BINARY);
    return { binaryPath: TARGET_DEBUG_BINARY };
  }
  const build = await runCargoBuild();
  if (await isExecutable(TARGET_DEBUG_BINARY)) {
    await warmMacosExec(TARGET_DEBUG_BINARY);
    return { binaryPath: TARGET_DEBUG_BINARY };
  }
  if (await isExecutable(FALLBACK_BINARY)) {
    await warmMacosExec(FALLBACK_BINARY);
    return { binaryPath: FALLBACK_BINARY };
  }
  const skipReason = build.ok
    ? `aft binary not found at ${relative(PROJECT_ROOT, TARGET_DEBUG_BINARY)} or ${FALLBACK_BINARY}`
    : `cargo build failed and no fallback aft binary was found\n${build.output}`;

  // In CI the aft binary is always built before the Bun suites run, so a missing
  // binary there means setup broke — fail loud instead of silently
  // `describe.skipIf(!binaryPath)`-ing e2e coverage into a false green. Locally
  // (CI unset) keep the quiet skip so the non-e2e suites still run without a build.
  if (process.env.CI === "true") {
    throw new Error(
      `e2e setup failed: ${skipReason}\n` +
        "The aft binary must be present in CI (built before Bun tests run). " +
        "Refusing to silently skip e2e coverage.",
    );
  }

  return {
    binaryPath: null,
    skipReason,
  };
}

async function isExecutable(filePath: string): Promise<boolean> {
  try {
    await access(filePath, constants.X_OK);
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
    child.stdout?.on("data", (c: Buffer) => {
      stdout += c.toString("utf8");
    });
    child.stderr?.on("data", (c: Buffer) => {
      stderr += c.toString("utf8");
    });
    child.on("error", (err) => {
      resolveBuild({ ok: false, output: err.message });
    });
    child.on("close", (code) => {
      resolveBuild({ ok: code === 0, output: `${stdout}${stderr}`.trim() });
    });
  });
}
