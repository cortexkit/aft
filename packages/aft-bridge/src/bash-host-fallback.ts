import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { join } from "node:path";
import { withPathPrepended } from "./path-env.js";
import { resolveCortexKitStorageRoot, resolveStoragePath } from "./storage-paths.js";

export const BASH_HOST_FALLBACK_BANNER =
  "[AFT host fallback - module transport down; no rewrites/compression/background]";
export const BASH_HOST_FALLBACK_MAX_OUTPUT_BYTES = 100 * 1024;
export const BASH_HOST_FALLBACK_MAX_TIMEOUT_MS = 10 * 60 * 1000;
export const BASH_HOST_FALLBACK_REFUSAL =
  "AFT transport is down; only foreground execution is available in host fallback";

export interface BashHostFallbackOptions {
  command: string;
  projectRoot: string;
  timeoutMs?: number;
  signal?: AbortSignal;
  env?: NodeJS.ProcessEnv;
}

export interface BashHostFallbackResult extends Record<string, unknown> {
  success: true;
  output: string;
  exit_code: number;
  truncated: boolean;
}

/**
 * Host fallback spawns with the raw host environment, which would put the
 * REAL `gh` in front of any governed repository's speech commands (the shim
 * normally rides the daemon-injected child PATH that fallback bypasses). Keep
 * the shims directory in front here too: the shim passes mechanical reads
 * through to the real gh without a daemon, and fails governed verbs closed
 * while the transport is down - exactly the fallback state. Without this, a
 * transport outage silently converts bot speech into ambient-credential posts.
 */
function pathKeyForPlatform(env: NodeJS.ProcessEnv, platform: NodeJS.Platform): string | undefined {
  return platform === "win32"
    ? Object.keys(env).find((key) => key.toLowerCase() === "path")
    : "PATH";
}

function mergeEnvForPlatform(
  env: NodeJS.ProcessEnv,
  overrides: NodeJS.ProcessEnv | undefined,
  platform: NodeJS.Platform,
): NodeJS.ProcessEnv {
  const merged = { ...env };
  if (platform === "win32" && overrides) {
    const overridesPath = Object.keys(overrides).some((key) => key.toLowerCase() === "path");
    if (overridesPath) {
      for (const key of Object.keys(merged)) {
        if (key.toLowerCase() === "path") delete merged[key];
      }
    }
  }
  return Object.assign(merged, overrides);
}

function hostFallbackEnvWithShims(
  env: NodeJS.ProcessEnv,
  platform: NodeJS.Platform,
): NodeJS.ProcessEnv {
  const normalized = withPathPrepended(env, undefined, platform);
  const pathKey = pathKeyForPlatform(normalized, platform);
  const inherited = pathKey === undefined ? undefined : normalized[pathKey];

  // Honor the caller-visible AFT_STORAGE_DIR override from the SAME env the
  // child will receive, falling back to the shared storage root.
  const storageRoot = normalized.AFT_STORAGE_DIR
    ? resolveStoragePath(normalized.AFT_STORAGE_DIR)
    : resolveCortexKitStorageRoot();
  const shimsDir = join(storageRoot, "shims");
  if (!existsSync(join(shimsDir, "gh"))) return normalized;

  const separator = platform === "win32" ? ";" : ":";
  const entries = (inherited ?? "").split(separator).filter((entry) => entry.length > 0);
  if (entries[0] === shimsDir) return normalized;
  normalized[pathKey ?? "PATH"] = [shimsDir, ...entries.filter((entry) => entry !== shimsDir)].join(
    separator,
  );
  return normalized;
}

export function hostFallbackPathWithShims(
  env: NodeJS.ProcessEnv,
  platform: NodeJS.Platform = process.platform,
): string | undefined {
  const normalized = hostFallbackEnvWithShims(env, platform);
  const pathKey = pathKeyForPlatform(normalized, platform);
  return pathKey === undefined ? undefined : normalized[pathKey];
}

export function bashHostFallbackAskPattern(command: string, cwd: string): string {
  return `AFT UNAVAILABLE - host fallback execution:\n\nExact command:\n${command}\n\nWorking directory:\n${cwd}`;
}

function appendTail(chunks: Buffer[], chunk: Buffer): { chunks: Buffer[]; truncated: boolean } {
  const combined = Buffer.concat([...chunks, chunk]);
  if (combined.byteLength <= BASH_HOST_FALLBACK_MAX_OUTPUT_BYTES) {
    return { chunks: [combined], truncated: false };
  }
  return {
    chunks: [combined.subarray(combined.byteLength - BASH_HOST_FALLBACK_MAX_OUTPUT_BYTES)],
    truncated: true,
  };
}

function renderOutput(output: Buffer, exitCode: number): string {
  const body = output.toString("utf8");
  const separator = body.length === 0 || body.endsWith("\n") ? "" : "\n";
  return `${BASH_HOST_FALLBACK_BANNER}\n${body}${separator}[exit code: ${exitCode}]`;
}

/** Execute one explicitly-approved shell command without any AFT processing. */
export async function runBashHostFallback(
  options: BashHostFallbackOptions,
): Promise<BashHostFallbackResult> {
  const timeoutMs = Math.min(
    Math.max(1, options.timeoutMs ?? BASH_HOST_FALLBACK_MAX_TIMEOUT_MS),
    BASH_HOST_FALLBACK_MAX_TIMEOUT_MS,
  );

  if (options.signal?.aborted) {
    throw new DOMException("The host fallback command was aborted", "AbortError");
  }

  return await new Promise<BashHostFallbackResult>((resolve, reject) => {
    const child = spawn(options.command, {
      cwd: options.projectRoot,
      shell: true,
      env: hostFallbackEnvWithShims(
        mergeEnvForPlatform(process.env, options.env, process.platform),
        process.platform,
      ),
      stdio: ["ignore", "pipe", "pipe"],
      detached: process.platform !== "win32",
      windowsHide: true,
    });

    let chunks: Buffer[] = [];
    let truncated = false;
    let timedOut = false;
    let aborted = false;
    let settled = false;
    let abortForceTimer: ReturnType<typeof setTimeout> | undefined;

    const capture = (chunk: Buffer | string) => {
      const next = appendTail(chunks, Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
      chunks = next.chunks;
      truncated ||= next.truncated;
    };
    child.stdout?.on("data", capture);
    child.stderr?.on("data", capture);

    const kill = (signal: NodeJS.Signals) => {
      if (child.exitCode !== null || child.signalCode !== null) return;
      if (process.platform !== "win32" && child.pid !== undefined) {
        try {
          process.kill(-child.pid, signal);
          return;
        } catch {
          // The process may have exited between the liveness check and group kill.
        }
      }
      child.kill(signal);
    };
    const onAbort = () => {
      aborted = true;
      kill("SIGTERM");
      abortForceTimer = setTimeout(() => kill("SIGKILL"), 250);
      abortForceTimer.unref?.();
    };
    options.signal?.addEventListener("abort", onAbort, { once: true });

    const timer = setTimeout(() => {
      timedOut = true;
      kill("SIGKILL");
    }, timeoutMs);

    const cleanup = () => {
      clearTimeout(timer);
      if (abortForceTimer !== undefined) clearTimeout(abortForceTimer);
      options.signal?.removeEventListener("abort", onAbort);
    };

    child.once("error", (error) => {
      if (settled) return;
      settled = true;
      cleanup();
      reject(error);
    });
    child.once("close", (code) => {
      if (settled) return;
      settled = true;
      cleanup();
      if (aborted) {
        reject(new DOMException("The host fallback command was aborted", "AbortError"));
        return;
      }
      const exitCode = timedOut ? 124 : (code ?? 1);
      resolve({
        success: true,
        output: renderOutput(Buffer.concat(chunks), exitCode),
        exit_code: exitCode,
        truncated,
      });
    });
  });
}
