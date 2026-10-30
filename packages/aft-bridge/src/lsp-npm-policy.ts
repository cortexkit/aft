import { stat } from "node:fs/promises";
import type { BridgeClock } from "./bridge.js";
import { spawn } from "./child-process.js";
import {
  type NpmInvocation,
  npmInvocation,
  npmSpawnEnv,
  type ResolvedNpm,
  terminateNpmProcessTree,
} from "./npm-resolver.js";

type NpmProbeOutcome = "supported" | "unsupported" | "transient";
interface NpmExecutableIdentity {
  size: number;
  mtimeMs: number;
}
interface NpmCutoffEntry {
  identity: NpmExecutableIdentity | undefined;
  pending: Promise<NpmProbeOutcome>;
  settled?: { outcome: NpmProbeOutcome; retryAt: number };
}

const npmCutoffSupport = new Map<string, NpmCutoffEntry>();
const NPM_PROBE_TIMEOUT_MS = 5_000;
const NPM_PROBE_OUTPUT_LIMIT = 1_048_576;
const NPM_PROBE_RETRY_MS = 5 * 60_000;
const SYSTEM_CLOCK: BridgeClock = {
  now: () => Date.now(),
  setTimeout: (callback, ms) => setTimeout(callback, ms),
  clearTimeout: (timer) => clearTimeout(timer as ReturnType<typeof setTimeout>),
};

function probeNpmCutoffSupport(npm: ResolvedNpm, clock: BridgeClock): Promise<NpmProbeOutcome> {
  const invocation = npmInvocation(npm, ["--version"]);
  return new Promise((resolve) => {
    const child = spawn(invocation.command, invocation.args, {
      stdio: ["ignore", "pipe", "pipe"],
      env: { ...npmSpawnEnv(npm), ...invocation.env },
      windowsVerbatimArguments: invocation.windowsVerbatimArguments,
    });
    child.unref();
    let stdout = "";
    let stdoutBytes = 0;
    let stderrBytes = 0;
    let settled = false;
    let stopping = false;
    const finish = (outcome: NpmProbeOutcome) => {
      if (settled) return;
      settled = true;
      clock.clearTimeout(deadline);
      child.stdout?.destroy();
      child.stderr?.destroy();
      resolve(outcome);
    };
    const stop = () => {
      if (settled || stopping) return;
      stopping = true;
      // Kill the spawned process immediately; Windows command shims need their
      // whole process tree terminated before their inherited pipes close.
      if (!invocation.windowsCmdShim) {
        try {
          child.kill("SIGKILL");
        } catch {
          // The child may have exited just before the deadline.
        }
      }
      void terminateNpmProcessTree(child, invocation, process.env, 500).then(
        () => finish("transient"),
        () => finish("transient"),
      );
    };
    const deadline = clock.setTimeout(stop, NPM_PROBE_TIMEOUT_MS);
    child.stdout?.setEncoding("utf8");
    child.stdout?.on("data", (chunk: string) => {
      if (settled || stopping) return;
      stdoutBytes += Buffer.byteLength(chunk);
      if (stdoutBytes > NPM_PROBE_OUTPUT_LIMIT) stop();
      else stdout += chunk;
    });
    child.stderr?.setEncoding("utf8");
    child.stderr?.on("data", (chunk: string) => {
      stderrBytes += Buffer.byteLength(chunk);
      if (stderrBytes > NPM_PROBE_OUTPUT_LIMIT) stop();
    });
    child.once("error", () => finish("transient"));
    child.once("close", (code) => {
      const major = Number(/^([0-9]+)\.[0-9]+\.[0-9]+(?:\s|$)/.exec(stdout.trim())?.[1]);
      if (stopping || code !== 0 || !Number.isSafeInteger(major)) finish("transient");
      else finish(major >= 7 ? "supported" : "unsupported");
    });
  });
}

async function npmSupportsCutoff(npm: ResolvedNpm, clock: BridgeClock): Promise<NpmProbeOutcome> {
  const identity = await stat(npm.command)
    .then(({ size, mtimeMs }) => ({ size, mtimeMs }))
    .catch(() => undefined);
  const existing = npmCutoffSupport.get(npm.command);
  const sameIdentity =
    existing &&
    existing.identity?.size === identity?.size &&
    existing.identity?.mtimeMs === identity?.mtimeMs;
  if (existing && sameIdentity) {
    // Pending callers share the probe. Only a clean, parseable version is a
    // durable verdict, and only while its executable identity remains known.
    if (
      !existing.settled ||
      (identity !== undefined && existing.settled.outcome !== "transient") ||
      clock.now() < existing.settled.retryAt
    )
      return existing.pending;
  }
  const pending = Promise.resolve()
    .then(() => probeNpmCutoffSupport(npm, clock))
    .catch((): NpmProbeOutcome => "transient")
    .then((outcome) => {
      // Update the captured entry, not the map: an executable replacement may
      // already have installed a newer in-flight probe for the same path.
      entry.settled = { outcome, retryAt: clock.now() + NPM_PROBE_RETRY_MS };
      return outcome;
    });
  const entry: NpmCutoffEntry = { identity, pending };
  npmCutoffSupport.set(npm.command, entry);
  return pending;
}

/** Unpinned installs exclude dependencies published inside the grace window; explicit pins bypass that filter. */
export function lspPublicationCutoff(
  graceDays: number,
  pinned: boolean,
  now = Date.now(),
): string | undefined {
  return pinned ? undefined : new Date(now - graceDays * 86_400_000).toISOString();
}

/** Suppress lifecycle scripts. Publication-date filtering requires npm 7+; unknown or older clients resolve to null rather than install. */
export async function lspNpmInstallInvocation(
  npm: ResolvedNpm,
  target: string,
  before?: string,
  clock: BridgeClock = SYSTEM_CLOCK,
): Promise<NpmInvocation | null> {
  if (before !== undefined && (await npmSupportsCutoff(npm, clock)) !== "supported") return null;
  return npmInvocation(npm, [
    "install",
    "--no-save",
    "--ignore-scripts",
    "--silent",
    ...(before ? [`--before=${before}`] : []),
    target,
  ]);
}
