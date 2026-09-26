/**
 * Ask an `aft` binary for its version without blocking the calling thread.
 *
 * `spawnSync`, `execFile` and `Bun.spawn` all start the child from the calling
 * thread, and on macOS that thread waits inside `posix_spawn` until the child
 * image is loaded. A cold 85 MB binary has kept a plugin host's only
 * JavaScript thread there for close to a minute. This module runs the
 * `--version` probe on a worker thread: the worker blocks in `posix_spawn`
 * while the host's event loop keeps serving, and the caller awaits the result.
 *
 * The worker source is passed inline (`eval: true`) so bundled plugin builds
 * do not need a separate worker file on disk. That inline source cannot import
 * the shared `./child-process.js` wrappers, so it sets `windowsHide: true`
 * itself: without it a host running with no console (a Windows background
 * service) gets a visible console window for every probe.
 */

import { Worker } from "node:worker_threads";

const DEFAULT_PROBE_TIMEOUT_MS = 60_000;

/**
 * Worker body. CommonJS because eval workers start as scripts in both Node and
 * Bun. The generous exec timeout matches the blocking probe: a first exec of a
 * just-written binary can take many seconds while macOS assesses it.
 */
const PROBE_WORKER_SOURCE = `
const { parentPort, workerData } = require("node:worker_threads");
const { spawnSync } = require("node:child_process");
let message;
try {
  const result = spawnSync(workerData.binaryPath, ["--version"], {
    encoding: "utf-8",
    stdio: ["ignore", "pipe", "pipe"],
    timeout: workerData.timeoutMs,
    windowsHide: true,
  });
  message = { stdout: result.stdout || "", stderr: result.stderr || "" };
} catch (err) {
  message = { stdout: "", stderr: "" };
}
parentPort.postMessage(message);
`;

/**
 * Parse `aft --version` output ("aft 0.9.0", on stdout or stderr) into the
 * bare version, or null when there is no output.
 */
export function parseAftVersionOutput(stdout: string, stderr: string): string | null {
  const raw = stdout.trim() || stderr.trim();
  if (!raw) return null;
  return raw.replace(/^aft\s+/, "");
}

type OffThreadProbe = (binaryPath: string, timeoutMs: number) => Promise<string | null>;

function probeOnWorker(binaryPath: string, timeoutMs: number): Promise<string | null> {
  return new Promise((resolve) => {
    let settled = false;
    let worker: Worker | undefined;
    const finish = (version: string | null) => {
      if (settled) return;
      settled = true;
      clearTimeout(guard);
      void worker?.terminate().catch(() => {});
      resolve(version);
    };
    // The exec timeout inside the worker should end the probe first; this
    // guard only covers a worker that never reports back at all.
    const guard = setTimeout(() => finish(null), timeoutMs + 5_000);
    try {
      worker = new Worker(PROBE_WORKER_SOURCE, {
        eval: true,
        workerData: { binaryPath, timeoutMs },
      });
    } catch {
      finish(null);
      return;
    }
    worker.on("message", (message: { stdout?: string; stderr?: string }) => {
      finish(parseAftVersionOutput(message.stdout ?? "", message.stderr ?? ""));
    });
    worker.on("error", () => finish(null));
    worker.on("exit", () => finish(null));
  });
}

let offThreadProbe: OffThreadProbe = probeOnWorker;

/** Test seam: observe or replace the worker-thread probe. Pass null to restore. */
export function __setOffThreadVersionProbeForTests(impl: OffThreadProbe | null): void {
  offThreadProbe = impl ?? probeOnWorker;
}

const inFlight = new Map<string, Promise<string | null>>();

/**
 * Read the version of `binaryPath` by executing `--version` on a worker
 * thread. Resolves to the bare version (e.g. `"0.57.2"`) or null when the
 * binary cannot be run or prints nothing. Concurrent probes of one path share
 * a single execution.
 */
export function readBinaryVersionOffThread(
  binaryPath: string,
  timeoutMs: number = DEFAULT_PROBE_TIMEOUT_MS,
): Promise<string | null> {
  const existing = inFlight.get(binaryPath);
  if (existing) return existing;
  const task = offThreadProbe(binaryPath, timeoutMs)
    .catch(() => null)
    .finally(() => {
      if (inFlight.get(binaryPath) === task) inFlight.delete(binaryPath);
    });
  inFlight.set(binaryPath, task);
  return task;
}
