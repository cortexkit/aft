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
import { homedir, tmpdir } from "node:os";
import { join, relative, resolve } from "node:path";
import type { BinaryBridge } from "@cortexkit/aft-bridge";
import { BridgePool, setActiveLogger } from "@cortexkit/aft-bridge";
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
import { registerLspTools } from "../../tools/lsp.js";
import { registerNavigateTool } from "../../tools/navigate.js";
import { registerReadingTools } from "../../tools/reading.js";
import { registerRefactorTool } from "../../tools/refactor.js";
import { registerSafetyTool } from "../../tools/safety.js";
import { registerSemanticTool } from "../../tools/semantic.js";
import { registerStructureTool } from "../../tools/structure.js";
import type { PluginContext } from "../../types.js";

const TARGET_DEBUG_BINARY = resolve(import.meta.dir, "../../../../../target/debug/aft");
const FALLBACK_BINARY = resolve(homedir(), ".cargo/bin/aft");
const PROJECT_ROOT = resolve(import.meta.dir, "../../../../../");
const FIXTURES_DIR = resolve(import.meta.dir, "./fixtures");
const DEFAULT_TIMEOUT_MS = 15_000;

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

  const tempDir = await mkdtemp(join(tmpdir(), "aft-pi-e2e-"));
  const previousCacheDir = process.env.AFT_CACHE_DIR;
  // Redirect AFT caches/indexes to temp so tests don't pollute user data.
  process.env.AFT_CACHE_DIR = join(tempDir, ".aft-cache");

  try {
    if (!options.noFixtures) {
      await copyFixturesToTempDir(tempDir, options.fixtureNames);
    }
  } catch (err) {
    restoreAftCacheDir(previousCacheDir);
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

  const pool = new BridgePool(
    preparedBinary.binaryPath,
    { timeoutMs: options.timeoutMs ?? DEFAULT_TIMEOUT_MS },
    // Forward the full config to configure so indexing/restrict/etc. match prod.
    { ...config, storage_dir: join(tempDir, ".aft-storage"), harness: "pi" },
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
    lspDiagnostics: true,
    structure: true,
    refactor: true,
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
  registerLspTools(api, ctx);
  registerStructureTool(api, ctx);
  registerRefactorTool(api, ctx);

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
        try {
          await rm(tempDir, { recursive: true, force: true }).catch(() => {});
        } finally {
          restoreAftCacheDir(previousCacheDir);
        }
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
    const child = spawn("git", args, { cwd, stdio: ["ignore", "pipe", "pipe"] });
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

function restoreAftCacheDir(previous: string | undefined): void {
  if (previous === undefined) {
    delete process.env.AFT_CACHE_DIR;
  } else {
    process.env.AFT_CACHE_DIR = previous;
  }
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
  if (await isExecutable(TARGET_DEBUG_BINARY)) {
    return { binaryPath: TARGET_DEBUG_BINARY };
  }
  const build = await runCargoBuild();
  if (await isExecutable(TARGET_DEBUG_BINARY)) {
    return { binaryPath: TARGET_DEBUG_BINARY };
  }
  if (await isExecutable(FALLBACK_BINARY)) {
    return { binaryPath: FALLBACK_BINARY };
  }
  return {
    binaryPath: null,
    skipReason: build.ok
      ? `aft binary not found at ${relative(PROJECT_ROOT, TARGET_DEBUG_BINARY)} or ${FALLBACK_BINARY}`
      : `cargo build failed and no fallback aft binary was found\n${build.output}`,
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
