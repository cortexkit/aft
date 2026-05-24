/**
 * Process-level shutdown handlers.
 *
 * Pi's `session_shutdown` event fires on normal session-end paths, but not when
 * the host Node process is killed by SIGTERM/SIGINT/SIGHUP. Without explicit
 * cleanup, OS propagates the signal to our `aft` children through the process
 * group, the bridge's `exit` handler fires, and (before the sibling fix in
 * bridge.ts) it would auto-restart into orphaned processes.
 *
 * This is a mirror of packages/opencode-plugin/src/shutdown-hooks.ts. The
 * `globalThis` guard ensures OS-level signal handlers are installed exactly
 * once per Node process, even if the Pi extension loads multiple times.
 */

import { log } from "./logger.js";

type Cleanup = () => Promise<void> | void;

interface GlobalState {
  cleanups: Set<Cleanup>;
  installed: boolean;
}

const GLOBAL_KEY = "__aftPiShutdownHooks__";

function getState(): GlobalState {
  const g = globalThis as unknown as Record<string, GlobalState | undefined>;
  if (!g[GLOBAL_KEY]) {
    g[GLOBAL_KEY] = { cleanups: new Set(), installed: false };
  }
  // biome-ignore lint/style/noNonNullAssertion: just initialized above
  return g[GLOBAL_KEY]!;
}

let shuttingDown = false;

async function runCleanups(reason: string): Promise<void> {
  if (shuttingDown) return;
  shuttingDown = true;
  const state = getState();
  if (state.cleanups.size === 0) return;
  log(`Shutdown triggered by ${reason} — running ${state.cleanups.size} cleanup(s)`);
  const cleanups = Array.from(state.cleanups);
  state.cleanups.clear();
  await Promise.allSettled(
    cleanups.map(async (fn) => {
      try {
        await fn();
      } catch (err) {
        log(`Cleanup error: ${(err as Error).message}`);
      }
    }),
  );
}

function installProcessHandlers(): void {
  const state = getState();
  if (state.installed) return;
  state.installed = true;

  const signals = ["SIGTERM", "SIGINT", "SIGHUP"] as const;
  for (const sig of signals) {
    process.on(sig, () => {
      void runCleanups(sig);
    });
  }
  process.on("beforeExit", () => {
    void runCleanups("beforeExit");
  });
}

/** Register a shutdown cleanup. Returns an unregister function. */
export function registerShutdownCleanup(fn: Cleanup): () => void {
  installProcessHandlers();
  const state = getState();
  state.cleanups.add(fn);
  return () => {
    state.cleanups.delete(fn);
  };
}
