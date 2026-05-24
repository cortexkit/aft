import { spawn } from "node:child_process";
import { constants, type Dirent } from "node:fs";
import { access, cp, mkdtemp, readdir, readFile, rm } from "node:fs/promises";
import { homedir, tmpdir } from "node:os";
import { join, relative, resolve } from "node:path";
import { BinaryBridge, type BridgeOptions, setActiveLogger } from "@cortexkit/aft-bridge";
import { bridgeLogger } from "../../logger.js";

// Route aft-bridge log calls (including forwarded Rust child stderr lines like
// "[aft] invalidated 7 files") into $TMPDIR/aft-plugin-test.log instead of
// console.error. Without this, every "invalidated N files" / "watcher started"
// line emitted by the Rust child during e2e tests leaks onto test stdout and
// pollutes the bash background-completion output preview.
setActiveLogger(bridgeLogger);

const TARGET_DEBUG_BINARY = resolve(import.meta.dir, "../../../../../target/debug/aft");
const FALLBACK_BINARY = resolve(homedir(), ".cargo/bin/aft");
const PROJECT_ROOT = resolve(import.meta.dir, "../../../../../");
const FIXTURES_DIR = resolve(import.meta.dir, "./fixtures");
const DEFAULT_TIMEOUT_MS = 15_000;
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

export interface E2EHarness {
  readonly binaryPath: string;
  readonly bridge: BinaryBridge;
  readonly tempDir: string;
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

export function prepareBinary(): Promise<PreparedBinary> {
  preparedBinaryPromise ??= prepareBinaryOnce();
  return preparedBinaryPromise;
}

export async function createHarness(
  preparedBinary: PreparedBinary,
  options?: {
    fixtureNames?: string[];
    timeoutMs?: number;
    tempPrefix?: string;
    bridgeOptions?: BridgeOptions;
  },
): Promise<E2EHarness> {
  if (!preparedBinary.binaryPath) {
    throw new Error(preparedBinary.skipReason ?? "aft binary unavailable");
  }

  const tempDir = await mkdtemp(join(tmpdir(), options?.tempPrefix ?? "aft-plugin-e2e-"));
  const previousCacheDir = process.env.AFT_CACHE_DIR;
  // Redirect search index cache to temp dir so tests don't pollute user's ~/.cache/aft/index/
  process.env.AFT_CACHE_DIR = join(tempDir, ".aft-cache");

  let bridge: BinaryBridge | undefined;
  try {
    await copyFixturesToTempDir(tempDir, options?.fixtureNames);

    bridge = new BinaryBridge(
      preparedBinary.binaryPath,
      tempDir,
      {
        timeoutMs: options?.timeoutMs ?? DEFAULT_TIMEOUT_MS,
        ...(options?.bridgeOptions ?? {}),
      },
      { harness: "opencode" },
    );
  } catch (err) {
    restoreAftCacheDir(previousCacheDir);
    await rm(tempDir, { recursive: true, force: true });
    throw err;
  }

  return {
    binaryPath: preparedBinary.binaryPath,
    bridge,
    tempDir,
    path: (...segments: string[]) => resolve(tempDir, ...segments),
    relativePath: (...segments: string[]) => segments.join("/"),
    cleanup: async () => {
      try {
        await bridge.shutdown();
      } catch {
        // ignore cleanup errors
      } finally {
        try {
          await rm(tempDir, { recursive: true, force: true });
        } finally {
          restoreAftCacheDir(previousCacheDir);
        }
      }
    },
  };
}

function restoreAftCacheDir(previous: string | undefined): void {
  if (previous === undefined) {
    delete process.env.AFT_CACHE_DIR;
  } else {
    process.env.AFT_CACHE_DIR = previous;
  }
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
  bridge: BinaryBridge,
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
  bridge: BinaryBridge,
  directory: string,
): Promise<Record<string, unknown>> {
  const files = await discoverOutlineFiles(directory);
  return bridge.send("outline", { files });
}

export async function sendZoomMultiSymbolLikePlugin(
  bridge: BinaryBridge,
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

export function fileResultBySuffix(
  response: Record<string, unknown>,
  suffix: string,
): Record<string, unknown> {
  const files = response.files as Array<Record<string, unknown>> | undefined;
  const match = files?.find((entry) => String(entry.file).endsWith(suffix));
  if (!match) {
    throw new Error(`Missing file result for suffix '${suffix}'`);
  }
  return match;
}

async function prepareBinaryOnce(): Promise<PreparedBinary> {
  if (await isExecutable(TARGET_DEBUG_BINARY)) {
    return {
      binaryPath: TARGET_DEBUG_BINARY,
      source: "target",
      buildAttempted: false,
    };
  }

  const build = await runCargoBuild();
  if (await isExecutable(TARGET_DEBUG_BINARY)) {
    return {
      binaryPath: TARGET_DEBUG_BINARY,
      source: "target",
      buildAttempted: true,
    };
  }

  if (await isExecutable(FALLBACK_BINARY)) {
    return {
      binaryPath: FALLBACK_BINARY,
      source: "fallback",
      buildAttempted: true,
    };
  }

  return {
    binaryPath: null,
    source: null,
    buildAttempted: true,
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
